//! Search suggestions — the app's three suggestion systems
//! (audit W-01), reproduced from the APK static analysis in
//! `baidu-search-analysis/` (`suggest-implementations.md`,
//! `sug-http-audit.md`, `sugrec-http-audit.md`,
//! `native-protocol-audit.md` §4):
//!
//! * **S1 classic sug** — the main input-box suggestion:
//!   POST form `data={}` to
//!   `https://m.baidu.com/suggest?ctl=sug_prefetch&query=…&v=3&…`.
//!   Measured in the 2026-10 analysis rounds: the Baidu front layer
//!   answered **HTTP 404** to the fully reproduced anonymous
//!   request, and the cause matrix left edge/session/server-change
//!   hypotheses open. The request is reproduced faithfully here;
//!   callers see the server's real answer (404 surfaces as
//!   `http_error`).
//! * **S2 SugRec** — recommendations shown when S1 is empty:
//!   POST form `data=<payload JSON>` to
//!   `https://m.baidu.com/suggest?ctl=cpage&action=universal&scene=errcpage`.
//!   Also measured HTTP 404 in the same rounds.
//! * **S4 AI input-box suggest** — POST form `scene=<text>` to
//!   `https://aigc.baidu.com/publisher/v1/ai/chat/inputbox`,
//!   JSON response (`errno == 0` carries `data`). Static analysis
//!   only in the original rounds; live behaviour is whatever the
//!   server answers now.
//!
//! Anonymous first-run state is reproduced: no history payload
//! (`data={}` / empty `guessdata`), no device-derived parameters.

use crate::errors::*;
use crate::urlbuild;
use serde::Serialize;
use std::time::Duration;
use std::time::Instant;
use tokio::sync::Mutex;

pub const SUG_HOST: &str = "https://m.baidu.com";
pub const INPUTBOX_URL: &str = "https://aigc.baidu.com/publisher/v1/ai/chat/inputbox";

// ---------------------------------------------------------------- S1

/// Build the S1 (classic sug) request URL for the anonymous
/// first-input state. Parameter order follows the app
/// (`sug-http-audit.md` §2): ctl, query, v=3, voi, wise_csor,
/// device_time, span_upgrade, event_type. Parameters the app only
/// adds when a value exists (lid, sugid, pq/pt, pwd, sug_mode,
/// hot_launch, action, prefetch set) are absent, exactly as on a
/// fresh anonymous install.
pub fn build_sug_url(query: &str, now_ms: i64) -> String {
    format!(
        "{}/suggest?ctl=sug_prefetch&query={}&v=3&voi=0&wise_csor={}&device_time={}&span_upgrade=1&event_type=0",
        SUG_HOST,
        urlbuild::java_urlencode(query),
        query.chars().count(),
        now_ms
    )
}

/// The S1 form body: a single `data` field. With no local history
/// (anonymous / private mode) the app sends the literal `{}`.
pub fn sug_form_body() -> Vec<(String, String)> {
    vec![("data".to_string(), "{}".to_string())]
}

/// One S1 candidate or special entry from the response `data` array
/// (field names from `o93.j` in the APK: `type`, `word`, `sa`,
/// `data`). Entries whose `type` falls in 1000..=9999 are special
/// cards in the app; plain suggestions carry a non-empty `word`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SugEntry {
    pub word: Option<String>,
    pub sa: Option<String>,
    #[serde(rename = "type")]
    pub item_type: Option<i64>,
    pub data: Option<String>,
}

/// Parse an S1 response body. The app scans the top-level `data`
/// array; a non-JSON or data-less body yields an empty list.
pub fn parse_sug_response(body: &str) -> Vec<SugEntry> {
    let value: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let data = match value.get("data").and_then(|d| d.as_array()) {
        Some(a) => a,
        None => return Vec::new(),
    };
    data.iter()
        .map(|item| SugEntry {
            word: item
                .get("word")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string()),
            sa: item
                .get("sa")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            item_type: item.get("type").and_then(|v| v.as_i64()),
            data: item
                .get("data")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
        })
        .collect()
}

/// The plain-text suggestions of an S1 response: entries with a
/// `word` whose type is outside the special 1000..=9999 range
/// (the app's `o93.j.G` branch condition).
pub fn sug_words(entries: &[SugEntry]) -> Vec<String> {
    entries
        .iter()
        .filter(|e| match e.item_type {
            Some(t) => !(1000..=9999).contains(&t),
            None => true,
        })
        .filter_map(|e| e.word.clone())
        .collect()
}

// ---------------------------------------------------------------- S2

/// Build the S2 (SugRec) payload JSON for the `rec` target,
/// byte-faithful to the APK (`sugrec-http-audit.md` §3), including
/// the leading space in `event_type` (an original APK typo kept
/// verbatim) and the empty `guessdata` of an empty on-device cache.
pub fn build_sugrec_rec_payload(query: &str) -> String {
    serde_json::json!({
        "query": query,
        "url": "https://m.baidu.com",
        "private_mode": 0,
        "target": "[\"rec\"]",
        "rec_max_return": 10,
        "event_type": " errcpage",
        "guessdata": "",
    })
    .to_string()
}

/// Build the S2 payload JSON for the `rs` target.
pub fn build_sugrec_rs_payload(query: &str) -> String {
    serde_json::json!({
        "query": query,
        "url": "https://m.baidu.com",
        "target": "[\"rs\"]",
        "rs_max_return": 10,
    })
    .to_string()
}

/// The S2 request URL (host is hardcoded in the app).
pub fn sugrec_url() -> String {
    format!(
        "{}/suggest?ctl=cpage&action=universal&scene=errcpage",
        SUG_HOST
    )
}

/// One S2 candidate (`term` for rs, `text` for rec; both carry `sa`).
#[derive(Debug, Clone, Default, Serialize)]
pub struct SugRecEntry {
    pub text: String,
    pub sa: Option<String>,
    /// Which list the entry came from: `rs` or `rec`.
    pub source: String,
}

/// Parse an S2 response body:
/// `{errno, requestid, data:{rs:{rslist:[{term,sa}]},
/// rec:{guess:{data:[{text,sa}]}}}}` (`sugrec-http-audit.md` §4).
pub fn parse_sugrec_response(body: &str) -> Vec<SugRecEntry> {
    let value: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let data = match value.get("data") {
        Some(d) => d,
        None => return Vec::new(),
    };
    let mut out = Vec::new();
    if let Some(list) = data
        .get("rs")
        .and_then(|r| r.get("rslist"))
        .and_then(|l| l.as_array())
    {
        for item in list {
            if let Some(term) = item.get("term").and_then(|v| v.as_str()) {
                out.push(SugRecEntry {
                    text: term.to_string(),
                    sa: item
                        .get("sa")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string()),
                    source: "rs".to_string(),
                });
            }
        }
    }
    if let Some(list) = data
        .get("rec")
        .and_then(|r| r.get("guess"))
        .and_then(|g| g.get("data"))
        .and_then(|l| l.as_array())
    {
        for item in list {
            if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                out.push(SugRecEntry {
                    text: text.to_string(),
                    sa: item
                        .get("sa")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string()),
                    source: "rec".to_string(),
                });
            }
        }
    }
    out
}

// ---------------------------------------------------------------- S4

/// The S4 form body: a single `scene` field holding the input text.
pub fn inputbox_form_body(query: &str) -> Vec<(String, String)> {
    vec![("scene".to_string(), query.to_string())]
}

/// One S4 suggestion (`SugInfo` in the APK).
#[derive(Debug, Clone, Default, Serialize)]
pub struct InputBoxSugInfo {
    pub content: Option<String>,
    pub id: Option<String>,
    pub mark: Option<String>,
    pub title: Option<String>,
    pub tool_name: Option<String>,
    #[serde(rename = "type")]
    pub item_type: Option<String>,
}

fn parse_sug_info(v: &serde_json::Value) -> InputBoxSugInfo {
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(|x| x.to_string());
    InputBoxSugInfo {
        content: s("content"),
        id: s("id"),
        mark: s("mark"),
        title: s("title"),
        tool_name: s("toolName"),
        item_type: s("type"),
    }
}

/// Parsed S4 response (`InputBoxSug` in the APK): the chat list
/// plus the content groups, flattened with their group name kept.
#[derive(Debug, Clone, Default, Serialize)]
pub struct InputBoxSug {
    pub chat_sug: Vec<InputBoxSugInfo>,
    /// (group, entries) for `contentSug`'s five lists
    /// (imageVideo / multipleImages / multipleVideo / singleImage /
    /// singleVideo), in that order, empty groups included.
    pub content_groups: Vec<(String, Vec<InputBoxSugInfo>)>,
}

/// Parse an S4 response body. Returns `Err` with the server's
/// `errno` / `msg` when `errno != 0` (the app treats that as a
/// business error, never as success).
pub fn parse_inputbox_response(body: &str) -> Result<InputBoxSug, BaiduError> {
    let value: serde_json::Value = serde_json::from_str(body).map_err(|e| {
        BaiduError::new(
            KIND_PARSE_ERROR,
            format!("inputbox response is not JSON: {}", e),
        )
    })?;
    let errno = value.get("errno").and_then(|v| v.as_i64()).unwrap_or(-1);
    if errno != 0 {
        let msg = value
            .get("msg")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        return Err(BaiduError::new(
            KIND_BAIDU_SEARCH_ERROR,
            format!("inputbox business error errno={} {}", errno, msg),
        ));
    }
    let data = value
        .get("data")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let chat_sug = data
        .get("chatSug")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().map(parse_sug_info).collect())
        .unwrap_or_default();
    let mut content_groups = Vec::new();
    if let Some(content) = data.get("contentSug") {
        for group in [
            "imageVideo",
            "multipleImages",
            "multipleVideo",
            "singleImage",
            "singleVideo",
        ] {
            let entries = content
                .get(group)
                .and_then(|v| v.as_array())
                .map(|a| a.iter().map(parse_sug_info).collect())
                .unwrap_or_default();
            content_groups.push((group.to_string(), entries));
        }
    }
    Ok(InputBoxSug {
        chat_sug,
        content_groups,
    })
}

// ---------------------------------------------------------------- client

/// POST client for the three suggestion systems, with the same
/// operating policy as the search fetcher: cookie-keeping
/// session, app UA, ≥ 1 s interval, no retries, 403/429 →
/// `access_restricted`, other ≥ 400 → `http_error`.
pub struct SuggestClient {
    client: reqwest::Client,
    min_interval: f64,
    last_request_at: Mutex<Option<Instant>>,
}

impl Default for SuggestClient {
    fn default() -> Self {
        Self::new(crate::client::DEFAULT_MIN_INTERVAL_SECS)
    }
}

impl SuggestClient {
    /// Create a suggest client with an explicit minimum interval between requests (seconds).
    pub fn new(min_interval: f64) -> Self {
        let client = reqwest::Client::builder()
            .cookie_store(true)
            .user_agent(urlbuild::USER_AGENT)
            .timeout(Duration::from_secs_f64(crate::client::DEFAULT_TIMEOUT_SECS))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        SuggestClient {
            client,
            min_interval,
            last_request_at: Mutex::new(None),
        }
    }

    async fn respect_interval(&self) {
        if self.min_interval <= 0.0 {
            return;
        }
        let wait = {
            let last = self.last_request_at.lock().await;
            match *last {
                Some(t) => {
                    let elapsed = t.elapsed().as_secs_f64();
                    if self.min_interval > elapsed {
                        self.min_interval - elapsed
                    } else {
                        0.0
                    }
                }
                None => 0.0,
            }
        };
        if wait > 0.0 {
            tokio::time::sleep(Duration::from_secs_f64(wait)).await;
        }
        *self.last_request_at.lock().await = Some(Instant::now());
    }

    async fn post_form(
        &self,
        url: &str,
        form: &[(String, String)],
        extra_headers: &[(&str, &str)],
    ) -> Result<String, BaiduError> {
        self.respect_interval().await;
        let mut req = self.client.post(url).form(form);
        for (k, v) in extra_headers {
            req = req.header(*k, *v);
        }
        let resp = req.send().await.map_err(|e| {
            crate::client::classify_error_chain(
                &e.to_string(),
                e.is_timeout(),
                e.is_redirect(),
                url,
                &e.to_string(),
            )
        })?;
        let status = resp.status().as_u16() as i64;
        let final_url = resp.url().to_string();
        let body = resp.text().await.map_err(|e| {
            crate::client::classify_error_chain(
                &e.to_string(),
                e.is_timeout(),
                e.is_redirect(),
                url,
                &e.to_string(),
            )
        })?;
        if status == 403 || status == 429 {
            return Err(BaiduError::new(
                KIND_ACCESS_RESTRICTED,
                format!("access restricted (HTTP {})", status),
            )
            .with_url(final_url)
            .with_status(status));
        }
        if status >= 400 {
            return Err(
                BaiduError::new(KIND_HTTP_ERROR, format!("HTTP error {}", status))
                    .with_url(final_url)
                    .with_status(status),
            );
        }
        Ok(body)
    }

    /// S1 classic sug. On the measured server behaviour this
    /// returns an `http_error` (404); see the module docs.
    pub async fn suggest(&self, query: &str) -> Result<Vec<SugEntry>, BaiduError> {
        let url = build_sug_url(query, crate::search_url::now_ms());
        let body = self
            .post_form(
                &url,
                &sug_form_body(),
                &[
                    ("new-style", "1"),
                    ("parallel-load", "1"),
                    ("X-Requested-With", "com.baidu.searchbox"),
                    ("X-Use-Search-BFF", "1"),
                    ("br_press", "1"),
                ],
            )
            .await?;
        Ok(parse_sug_response(&body))
    }

    /// S2 SugRec recommendations (`rec` target).
    pub async fn suggest_recommendations(
        &self,
        query: &str,
    ) -> Result<Vec<SugRecEntry>, BaiduError> {
        let form = vec![("data".to_string(), build_sugrec_rec_payload(query))];
        let body = self.post_form(&sugrec_url(), &form, &[]).await?;
        Ok(parse_sugrec_response(&body))
    }

    /// S4 AI input-box suggestions.
    pub async fn suggest_inputbox(&self, query: &str) -> Result<InputBoxSug, BaiduError> {
        let body = self
            .post_form(INPUTBOX_URL, &inputbox_form_body(query), &[])
            .await?;
        parse_inputbox_response(&body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sug_url_shape() {
        let url = build_sug_url("天气", 1700000000000);
        assert_eq!(
            url,
            "https://m.baidu.com/suggest?ctl=sug_prefetch&query=%E5%A4%A9%E6%B0%94&v=3&voi=0&wise_csor=2&device_time=1700000000000&span_upgrade=1&event_type=0"
        );
        assert_eq!(
            sug_form_body(),
            vec![("data".to_string(), "{}".to_string())]
        );
    }

    #[test]
    fn sug_response_words_and_specials() {
        // Field names per o93.j (type / word / sa / data); the
        // 1000..=9999 type range holds special cards, plain words
        // sit outside it.
        let body = r#"{"errno":"0","data":[
            {"type":0,"word":"天气","sa":"tpl_wd"},
            {"type":1018,"sa":"x","data":"payload"},
            {"type":3,"word":"天气预报","sa":"y"}
        ]}"#;
        let entries = parse_sug_response(body);
        assert_eq!(entries.len(), 3);
        assert_eq!(sug_words(&entries), vec!["天气", "天气预报"]);
        assert_eq!(entries[1].item_type, Some(1018));
        assert_eq!(entries[1].data.as_deref(), Some("payload"));
        assert!(parse_sug_response("not json").is_empty());
        assert!(parse_sug_response("{}").is_empty());
    }

    #[test]
    fn sugrec_payloads_are_apk_faithful() {
        let rec = build_sugrec_rec_payload("abc");
        let v: serde_json::Value = serde_json::from_str(&rec).unwrap();
        assert_eq!(v["event_type"], " errcpage"); // leading space is the APK's own string
        assert_eq!(v["target"], "[\"rec\"]");
        assert_eq!(v["rec_max_return"], 10);
        let rs = build_sugrec_rs_payload("abc");
        let v: serde_json::Value = serde_json::from_str(&rs).unwrap();
        assert_eq!(v["target"], "[\"rs\"]");
        assert_eq!(v["rs_max_return"], 10);
    }

    #[test]
    fn sugrec_response_both_lists() {
        let body = r#"{"errno":0,"requestid":"r1","data":{
            "rs":{"rslist":[{"term":"t1","sa":"s1"}]},
            "rec":{"guess":{"data":[{"text":"t2","sa":"s2"}]}}
        }}"#;
        let entries = parse_sugrec_response(body);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].text, "t1");
        assert_eq!(entries[0].source, "rs");
        assert_eq!(entries[1].text, "t2");
        assert_eq!(entries[1].source, "rec");
    }

    #[test]
    fn inputbox_response_and_business_error() {
        let body = r#"{"errno":0,"data":{
            "chatSug":[{"content":"c1","id":"1","title":"t","type":"sug"}],
            "contentSug":{"singleImage":[{"content":"c2"}],"singleVideo":[]}
        }}"#;
        let parsed = parse_inputbox_response(body).unwrap();
        assert_eq!(parsed.chat_sug.len(), 1);
        assert_eq!(parsed.chat_sug[0].content.as_deref(), Some("c1"));
        assert_eq!(parsed.content_groups.len(), 5);
        let single_image = &parsed.content_groups[3];
        assert_eq!(single_image.0, "singleImage");
        assert_eq!(single_image.1.len(), 1);
        let err = parse_inputbox_response(r#"{"errno":201,"msg":"bad"}"#).unwrap_err();
        assert!(err.message.contains("errno=201"));
        assert_eq!(
            inputbox_form_body("hi"),
            vec![("scene".to_string(), "hi".to_string())]
        );
    }
}
