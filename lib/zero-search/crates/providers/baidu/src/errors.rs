//! Error taxonomy — faithful port of Python `baidu_search.errors`.
//!
//! Every failure carries a stable `kind` string identical to the
//! Python exception classes:
//! `dns_error`, `connection_error`, `tls_error`, `timeout`,
//! `redirect_error`, `response_too_large`, `http_error`,
//! `access_restricted`, `captcha_or_verification`,
//! `not_search_result`, `parse_error`, `pagination_cycle`,
//! `invalid_next_url`, `transport_error`, `pagination_error`,
//! `baidu_search_error` (base).

use serde_json::{json, Value};

pub const KIND_BAIDU_SEARCH_ERROR: &str = "baidu_search_error";
pub const KIND_TRANSPORT_ERROR: &str = "transport_error";
pub const KIND_DNS_ERROR: &str = "dns_error";
pub const KIND_CONNECTION_ERROR: &str = "connection_error";
pub const KIND_TLS_ERROR: &str = "tls_error";
pub const KIND_TIMEOUT: &str = "timeout";
pub const KIND_REDIRECT_ERROR: &str = "redirect_error";
pub const KIND_RESPONSE_TOO_LARGE: &str = "response_too_large";
pub const KIND_HTTP_ERROR: &str = "http_error";
pub const KIND_ACCESS_RESTRICTED: &str = "access_restricted";
pub const KIND_CAPTCHA: &str = "captcha_or_verification";
pub const KIND_NOT_SEARCH_RESULT: &str = "not_search_result";
pub const KIND_PARSE_ERROR: &str = "parse_error";
pub const KIND_PAGINATION_ERROR: &str = "pagination_error";
pub const KIND_PAGINATION_CYCLE: &str = "pagination_cycle";
pub const KIND_INVALID_NEXT_URL: &str = "invalid_next_url";

/// Mirror of Python `BaiduSearchError` (message + optional url/status).
#[derive(Debug, Clone)]
pub struct BaiduError {
    pub kind: String,
    pub message: String,
    pub url: Option<String>,
    pub status_code: Option<i64>,
}

impl BaiduError {
    /// Create an error with a stable `kind` string and a human-readable message.
    pub fn new(kind: &str, message: impl Into<String>) -> Self {
        BaiduError {
            kind: kind.to_string(),
            message: message.into(),
            url: None,
            status_code: None,
        }
    }
    /// Attach the URL associated with the failure.
    pub fn with_url(mut self, url: impl Into<String>) -> Self {
        self.url = Some(url.into());
        self
    }
    /// Attach the HTTP status code associated with the failure.
    pub fn with_status(mut self, status_code: i64) -> Self {
        self.status_code = Some(status_code);
        self
    }
    /// Python `BaiduSearchError.to_dict()`.
    pub fn to_value(&self) -> Value {
        json!({
            "kind": self.kind,
            "message": self.message,
            "url": self.url,
            "status_code": self.status_code,
        })
    }
}

impl std::fmt::Display for BaiduError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.kind, self.message)
    }
}
impl std::error::Error for BaiduError {}
