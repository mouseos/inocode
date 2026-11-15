//! Request handling for InoCode.
//!
//! A [`Request`] is decoded from a JSON frame payload and answered with a
//! [`Response`]. The core knows nothing about TCP, framing, or connections, so
//! the server owns all transport concerns and calls [`Core::handle`] once per
//! request.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// One request decoded from a JSON frame payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    /// Correlation id that the response must echo.
    pub id: u64,
    /// Method to run.
    pub method: String,
    /// Method arguments. `None` when the client omitted `params`.
    #[serde(default)]
    pub params: Option<Value>,
}

/// Response for one request.
#[derive(Debug, Clone, Serialize)]
pub struct Response {
    /// Echoed request id, or `null` when the payload was not a valid request.
    pub id: Option<u64>,
    /// Serialized next to `id`, so the payload is either `result` or `error`.
    #[serde(flatten)]
    pub body: ResponseBody,
}

/// The outcome of a request.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum ResponseBody {
    Result { result: Value },
    Error { error: ErrorInfo },
}

/// Failure reported to the client.
#[derive(Debug, Clone, Serialize)]
pub struct ErrorInfo {
    pub code: &'static str,
    pub message: String,
}

impl ErrorInfo {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// Request processor.
///
/// Holds no per-connection state, so one instance serves every connection. The
/// implemented methods (`ping`, `echo`) exist to exercise the request path and
/// are not an application API.
#[derive(Debug)]
pub struct Core;

impl Core {
    /// Runs `request` and returns the response to send back.
    pub fn handle(&self, request: Request) -> Response {
        let Request { id, method, params } = request;
        let body = match method.as_str() {
            "ping" => ResponseBody::Result {
                result: json!({ "pong": true }),
            },
            "echo" => match params {
                Some(value) => ResponseBody::Result { result: value },
                None => ResponseBody::Error {
                    error: ErrorInfo::new("invalid_params", "echo requires params"),
                },
            },
            _ => ResponseBody::Error {
                error: ErrorInfo::new("unknown_method", "Unknown method"),
            },
        };
        Response { id: Some(id), body }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(id: u64, method: &str, params: Option<Value>) -> Request {
        Request {
            id,
            method: method.to_string(),
            params,
        }
    }

    #[test]
    fn ping_answers_with_pong_and_echoes_the_id() {
        let response = Core.handle(request(7, "ping", None));
        assert_eq!(response.id, Some(7));
        assert!(matches!(
            response.body,
            ResponseBody::Result { result } if result == json!({ "pong": true })
        ));
    }

    #[test]
    fn echo_returns_the_sent_value() {
        let params = json!({ "text": "日本語", "numbers": [1, 2, 3] });
        let response = Core.handle(request(1, "echo", Some(params.clone())));
        assert!(matches!(
            response.body,
            ResponseBody::Result { result } if result == params
        ));
    }

    #[test]
    fn echo_without_params_is_an_error() {
        let response = Core.handle(request(3, "echo", None));
        assert_eq!(response.id, Some(3));
        assert!(matches!(
            response.body,
            ResponseBody::Error { error } if error.code == "invalid_params"
        ));
    }

    #[test]
    fn unknown_method_is_rejected() {
        let response = Core.handle(request(9, "delete_everything", None));
        assert_eq!(response.id, Some(9));
        assert!(matches!(
            response.body,
            ResponseBody::Error { error }
                if error.code == "unknown_method" && error.message == "Unknown method"
        ));
    }
}
