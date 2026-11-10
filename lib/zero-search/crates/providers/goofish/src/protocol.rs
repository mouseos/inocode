//! MTOP wire-format assembly — port of `goofish_client/protocol.py`
//! (offline assembly only; sending lives in the client).

use anyhow::{anyhow, Result};
use serde_json::{json, Map, Value};

use crate::requests::RequestPlan;

pub const GATEWAY_PRIMARY: &str = "https://acs.m.goofish.com";
pub const GATEWAY_FALLBACK: &str = "https://acs.wapa.goofish.com";
pub const PUBLIC_APP_KEY: &str = "21407387";

#[derive(Debug, Clone, Default)]
pub struct RuntimeContext {
    pub sign: Option<String>,
    pub timestamp: Option<String>,
    pub utdid: Option<String>,
    pub device_id: Option<String>,
    pub umid_token: Option<String>,
    pub sid: Option<String>,
    pub uid: Option<String>,
    pub ttid: Option<String>,
    pub pv: Option<String>,
    pub features: Option<String>,
    pub app_version: Option<String>,
}

impl RuntimeContext {
    pub const REQUIRED: [&'static str; 4] = ["sign", "timestamp", "utdid", "device_id"];

    fn get(&self, name: &str) -> Option<&String> {
        match name {
            "sign" => self.sign.as_ref(),
            "timestamp" => self.timestamp.as_ref(),
            "utdid" => self.utdid.as_ref(),
            "device_id" => self.device_id.as_ref(),
            _ => None,
        }
    }

    /// Missing required fields. `device_id` is missing only when None —
    /// "" is a supplied value (the fresh-install, pre-registration
    /// state, measured accepted in round 19); every other required
    /// field is missing when falsy.
    pub fn missing(&self) -> Vec<String> {
        let mut missing = Vec::new();
        for name in Self::REQUIRED {
            let value = self.get(name);
            let is_missing = if name == "device_id" {
                value.is_none()
            } else {
                value.map(|v| v.is_empty()).unwrap_or(true)
            };
            if is_missing {
                missing.push(name.to_string());
            }
        }
        missing
    }
}

pub fn build_url(plan: &RequestPlan, gateway: &str) -> String {
    format!("{gateway}/gw/{}/{}/", plan.api_name, plan.version)
}

/// Assemble `{url_path, headers, form}` for a request. Errors with the
/// missing-parameter list when the runtime context is absent or
/// incomplete — the client stops rather than fabricating values.
pub fn build_form(plan: &RequestPlan, ctx: Option<&RuntimeContext>) -> Result<Value> {
    let ctx = match ctx {
        None => {
            return Err(anyhow!(
                "missing runtime parameters required for a live MTOP request: {}",
                RuntimeContext::REQUIRED.join(", ")
            ))
        }
        Some(c) => c,
    };
    let missing = ctx.missing();
    if !missing.is_empty() {
        return Err(anyhow!(
            "missing runtime parameters required for a live MTOP request: {}",
            missing.join(", ")
        ));
    }
    let mut headers = Map::new();
    headers.insert("x-t".into(), json!(ctx.timestamp.clone().unwrap()));
    headers.insert("x-sign".into(), json!(ctx.sign.clone().unwrap()));
    headers.insert("x-appkey".into(), json!(PUBLIC_APP_KEY));
    headers.insert("x-utdid".into(), json!(ctx.utdid.clone().unwrap()));
    headers.insert("x-devid".into(), json!(ctx.device_id.clone().unwrap()));
    headers.insert("x-sid".into(), json!(ctx.sid.clone().unwrap_or_default()));
    headers.insert("x-uid".into(), json!(ctx.uid.clone().unwrap_or_default()));
    headers.insert("f-refer".into(), json!("mtop"));
    if let Some(pv) = &ctx.pv {
        if !pv.is_empty() {
            headers.insert("x-pv".into(), json!(pv));
        }
    }
    if let Some(f) = &ctx.features {
        if !f.is_empty() {
            headers.insert("x-features".into(), json!(f));
        }
    }
    if let Some(av) = &ctx.app_version {
        if !av.is_empty() {
            headers.insert("x-app-ver".into(), json!(av));
        }
    }
    if let Some(u) = &ctx.umid_token {
        if !u.is_empty() {
            headers.insert("x-umt".into(), json!(u));
        }
    }
    if let Some(t) = &ctx.ttid {
        if !t.is_empty() {
            headers.insert("x-ttid".into(), json!(t));
        }
    }
    let mut form = Map::new();
    form.insert("data".into(), json!(plan.data_json()));
    Ok(json!({
        "url_path": format!("/gw/{}/{}/", plan.api_name, plan.version),
        "headers": Value::Object(headers),
        "form": Value::Object(form),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::requests::{build_search_request, SearchOptions};

    // Ported from goofish-client/tests/test_protocol_transport.py
    #[test]
    fn build_url_test() {
        let plan = build_search_request(Some("x"), &SearchOptions::default()).unwrap();
        assert_eq!(
            build_url(&plan, GATEWAY_PRIMARY),
            "https://acs.m.goofish.com/gw/mtop.taobao.idlemtopsearch.search/1.0/"
        );
        assert!(GATEWAY_FALLBACK.contains("wapa.goofish.com"));
    }

    #[test]
    fn build_form_requires_runtime_context() {
        let plan = build_search_request(Some("x"), &SearchOptions::default()).unwrap();
        let err = build_form(&plan, None).unwrap_err().to_string();
        for name in ["sign", "timestamp", "utdid", "device_id"] {
            assert!(err.contains(name), "missing list must name {name}: {err}");
        }
        let ctx = RuntimeContext {
            sign: Some("s".into()),
            ..Default::default()
        };
        let err = build_form(&plan, Some(&ctx)).unwrap_err().to_string();
        assert!(err.contains("timestamp") && err.contains("utdid") && err.contains("device_id"));
        assert!(!err.contains("sign,"));
    }

    #[test]
    fn build_form_assembly_with_injected_context() {
        let ctx = RuntimeContext {
            sign: Some("SYNTHETIC".into()),
            timestamp: Some("1700000000000".into()),
            utdid: Some("UTDID-TEST".into()),
            device_id: Some("DEV-TEST".into()),
            ..Default::default()
        };
        let plan = build_search_request(Some("x"), &SearchOptions::default()).unwrap();
        let form = build_form(&plan, Some(&ctx)).unwrap();
        assert_eq!(
            form["url_path"],
            json!("/gw/mtop.taobao.idlemtopsearch.search/1.0/")
        );
        let headers = &form["headers"];
        assert_eq!(headers["x-appkey"], json!("21407387"));
        assert_eq!(headers["x-sign"], json!("SYNTHETIC"));
        assert!(headers.get("x-umt").is_none() && headers.get("x-ttid").is_none());
        assert_eq!(headers["x-sid"], json!(""));
        assert_eq!(headers["x-uid"], json!(""));
        assert_eq!(headers["f-refer"], json!("mtop"));
        let form_fields = form["form"].as_object().unwrap();
        assert_eq!(form_fields.len(), 1);
        assert!(form_fields.contains_key("data"));
    }

    #[test]
    fn device_id_empty_string_is_supplied() {
        let ctx = RuntimeContext {
            sign: Some("s".into()),
            timestamp: Some("1".into()),
            utdid: Some("u".into()),
            device_id: Some(String::new()),
            ..Default::default()
        };
        assert!(ctx.missing().is_empty());
        let ctx2 = RuntimeContext {
            sign: Some("s".into()),
            timestamp: Some("1".into()),
            utdid: Some(String::new()),
            device_id: Some("d".into()),
            ..Default::default()
        };
        assert_eq!(ctx2.missing(), vec!["utdid".to_string()]);
    }
}
