//! Error taxonomy, aligned with the other ZeroSearch providers
//! (`{provider, ok:false, error:{kind, message, ...}}`).

use serde_json::{json, Value};

pub const KIND_TRANSPORT_ERROR: &str = "transport_error";
pub const KIND_TIMEOUT: &str = "timeout";
pub const KIND_HTTP_ERROR: &str = "http_error";
pub const KIND_ACCESS_RESTRICTED: &str = "access_restricted";
pub const KIND_CAPTCHA: &str = "captcha_or_verification";
pub const KIND_PARSE_ERROR: &str = "parse_error";
pub const KIND_NOT_FOUND: &str = "not_found";
pub const KIND_INVALID_ARGUMENT: &str = "invalid_argument";
pub const KIND_API_ERROR: &str = "api_error";

/// Provider error carrying a stable `kind` string.
#[derive(Debug, Clone)]
pub struct YoutubeError {
    pub kind: String,
    pub message: String,
    pub status_code: Option<i64>,
}

impl YoutubeError {
    pub fn new(kind: &str, message: impl Into<String>) -> Self {
        YoutubeError {
            kind: kind.to_string(),
            message: message.into(),
            status_code: None,
        }
    }
    pub fn with_status(mut self, status_code: i64) -> Self {
        self.status_code = Some(status_code);
        self
    }
    /// The `error` object of the standard result envelope.
    pub fn to_value(&self) -> Value {
        json!({
            "kind": self.kind,
            "message": self.message,
            "status_code": self.status_code,
        })
    }
}

impl std::fmt::Display for YoutubeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.kind, self.message)
    }
}
impl std::error::Error for YoutubeError {}

impl From<reqwest::Error> for YoutubeError {
    fn from(e: reqwest::Error) -> Self {
        if e.is_timeout() {
            YoutubeError::new(KIND_TIMEOUT, e.to_string())
        } else if e.is_status() {
            let status = e.status().map(|s| s.as_u16() as i64);
            let mut err = YoutubeError::new(KIND_HTTP_ERROR, e.to_string());
            err.status_code = status;
            err
        } else {
            YoutubeError::new(KIND_TRANSPORT_ERROR, e.to_string())
        }
    }
}
