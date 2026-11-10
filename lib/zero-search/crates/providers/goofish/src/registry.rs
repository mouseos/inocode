//! Bundled API registry (verbatim copy of the Python package data,
//! `goofish_client/data/api_registry.json`, 45 rows), embedded with
//! `include_str!` and used for API version / pagination / need-login
//! lookup exactly as `goofish_client.api_registry` does.

use anyhow::{anyhow, Result};
use serde_json::Value;
use std::sync::OnceLock;

pub const REGISTRY_JSON: &str = include_str!("data/api_registry.json");

#[derive(Debug, Clone)]
pub struct ApiSpec {
    pub api_name: String,
    pub version: String,
    pub pagination_kind: String,
    pub need_login: Option<bool>,
    pub parser_function: Option<String>,
    pub client_builder: Option<String>,
    pub excluded_reason: Option<String>,
    pub feature_id: String,
}

impl ApiSpec {
    pub fn is_login_required(&self) -> bool {
        self.need_login == Some(true) || self.excluded_reason.is_some()
    }
}

fn registry_doc() -> &'static Value {
    static DOC: OnceLock<Value> = OnceLock::new();
    DOC.get_or_init(|| {
        serde_json::from_str(REGISTRY_JSON).expect("bundled api_registry.json is valid JSON")
    })
}

pub fn registry_rows() -> Vec<ApiSpec> {
    let apis = registry_doc()
        .get("apis")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    apis.iter()
        .map(|row| ApiSpec {
            api_name: row
                .get("api_name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            version: row
                .get("version")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            pagination_kind: row
                .get("pagination_kind")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string(),
            need_login: row
                .get("need_login")
                .and_then(|v| v.get("value"))
                .and_then(|v| v.as_bool()),
            parser_function: row
                .get("parser_function")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            client_builder: row
                .get("client_builder")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            excluded_reason: row
                .get("excluded_reason")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            feature_id: row
                .get("feature_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        })
        .collect()
}

pub fn get_api(api_name: &str) -> Result<ApiSpec> {
    registry_rows()
        .into_iter()
        .find(|s| s.api_name == api_name)
        .ok_or_else(|| anyhow!("API not in registry: {api_name}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_has_45_rows() {
        assert_eq!(registry_rows().len(), 45);
    }

    #[test]
    fn registry_versions_match_python() {
        assert_eq!(
            get_api("mtop.taobao.idlemtopsearch.search")
                .unwrap()
                .version,
            "1.0"
        );
        assert_eq!(
            get_api("mtop.taobao.idlemtopsearch.search.suggest")
                .unwrap()
                .version,
            "4.0"
        );
        assert_eq!(
            get_api("mtop.taobao.idle.awesome.detail.unit")
                .unwrap()
                .version,
            "1.0"
        );
        assert_eq!(
            get_api("mtop.taobao.idle.category.guide").unwrap().version,
            "2.1"
        );
        assert_eq!(
            get_api("mtop.taobao.idle.comment.list").unwrap().version,
            "5.0"
        );
        assert_eq!(
            get_api("mtop.taobao.idle.rate.list").unwrap().version,
            "6.0"
        );
        assert_eq!(
            get_api("mtop.taobao.idlehome.home.nextfresh")
                .unwrap()
                .version,
            "8.0"
        );
        assert_eq!(
            get_api("com.taobao.idle.video.play").unwrap().version,
            "5.0"
        );
    }

    #[test]
    fn registry_pagination_and_login() {
        let search = get_api("mtop.taobao.idlemtopsearch.search").unwrap();
        assert_eq!(search.pagination_kind, "search_resultlist_nonempty");
        assert_eq!(search.need_login, Some(false));
        let attention = get_api("mtop.taobao.idle.attention.list").unwrap();
        assert!(attention.is_login_required());
        assert!(get_api("mtop.taobao.idle.does.not.exist").is_err());
    }
}
