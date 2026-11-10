//! MTOP response classification — a faithful port of
//! `goofish_client.errors.classify_mtop_response` and `ret_code_of`.

use serde_json::Value;

pub const CATEGORY_SUCCESS: &str = "success";
pub const CATEGORY_VERIFICATION_REQUIRED: &str = "verification_required";
pub const CATEGORY_ACCESS_DENIED: &str = "access_denied";
pub const CATEGORY_RATE_LIMITED: &str = "rate_limited";
pub const CATEGORY_LOGIN_REQUIRED: &str = "login_required";
pub const CATEGORY_SIGNING_OR_PROTOCOL: &str = "signing_or_protocol";
pub const CATEGORY_API_NOT_FOUND: &str = "api_not_found";
pub const CATEGORY_APP_VERSION_REQUIRED: &str = "app_version_required";
pub const CATEGORY_BUSINESS_ERROR: &str = "business_error";
pub const CATEGORY_API_ERROR: &str = "api_error";
pub const CATEGORY_HTTP_STATUS_ERROR: &str = "http_status_error";
pub const CATEGORY_UNKNOWN: &str = "unknown";

/// Categories that trip the client send-halt switch (errors.py
/// RESTRICTION_CATEGORIES).
pub fn is_restriction(category: &str) -> bool {
    matches!(
        category,
        CATEGORY_VERIFICATION_REQUIRED | CATEGORY_ACCESS_DENIED | CATEGORY_RATE_LIMITED
    )
}

/// The code part (before "::") of the first ret entry, upper-cased.
/// Returns "" when ret is empty or not a list.
pub fn ret_code_of(ret: &Value) -> String {
    let arr = match ret.as_array() {
        Some(a) if !a.is_empty() => a,
        _ => return String::new(),
    };
    let first = match &arr[0] {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    first.split("::").next().unwrap_or("").trim().to_uppercase()
}

pub fn classify_mtop_response(status_code: u16, ret: &Value) -> &'static str {
    let status = status_code as i64;
    let code = ret_code_of(ret);
    if code == "SUCCESS" {
        return if status < 400 {
            CATEGORY_SUCCESS
        } else {
            CATEGORY_HTTP_STATUS_ERROR
        };
    }
    if status == 419 || code == "FAIL_SYS_USER_VALIDATE" || code.contains("USER_VALIDATE") {
        return CATEGORY_VERIFICATION_REQUIRED;
    }
    if code == "FAIL_SYS_API_NOT_FOUNDED" || code == "FAIL_SYS_API_NOT_FOUND" {
        return CATEGORY_API_NOT_FOUND;
    }
    if status == 401
        || (code.starts_with("FAIL_SYS_")
            && (code.contains("SESSION") || code.contains("LOGIN") || code.contains("TOKEN")))
    {
        return CATEGORY_LOGIN_REQUIRED;
    }
    if code.starts_with("FAIL_SYS_") && (code.contains("PROTOVER") || code.contains("SIGN")) {
        return CATEGORY_SIGNING_OR_PROTOCOL;
    }
    if status == 429
        || code.contains("FLOWLIMIT")
        || code.contains("FLOW_LIMIT")
        || code == "FAIL_SYS_TRAFFIC_LIMIT"
        || code == "FAIL_SYS_RATE_LIMIT"
    {
        return CATEGORY_RATE_LIMITED;
    }
    if status == 403 || code.contains("FORBIDDEN") || code.contains("ACCESS_DENIED") {
        return CATEGORY_ACCESS_DENIED;
    }
    if code.starts_with("FAIL_BIZ_SERVER_NEED_UPGRADE") {
        return CATEGORY_APP_VERSION_REQUIRED;
    }
    if code.starts_with("FAIL_BIZ") {
        return CATEGORY_BUSINESS_ERROR;
    }
    if status >= 400 {
        return CATEGORY_HTTP_STATUS_ERROR;
    }
    if code.starts_with("FAIL") || code.contains("ERROR") || code.contains("ERR_") {
        return CATEGORY_API_ERROR;
    }
    CATEGORY_UNKNOWN
}

/// Categories that raise a typed error from `execute()` instead of
/// falling through to the parser (client.py `_RESPONSE_ERROR_TYPES`).
pub fn is_typed_error_category(category: &str) -> bool {
    matches!(
        category,
        CATEGORY_VERIFICATION_REQUIRED
            | CATEGORY_ACCESS_DENIED
            | CATEGORY_RATE_LIMITED
            | CATEGORY_LOGIN_REQUIRED
            | CATEGORY_SIGNING_OR_PROTOCOL
            | CATEGORY_API_NOT_FOUND
            | CATEGORY_APP_VERSION_REQUIRED
            | CATEGORY_HTTP_STATUS_ERROR
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn classify_table() {
        assert_eq!(
            classify_mtop_response(200, &json!(["SUCCESS::调用成功"])),
            CATEGORY_SUCCESS
        );
        assert_eq!(
            classify_mtop_response(419, &json!(["FAIL_SYS_USER_VALIDATE::x"])),
            CATEGORY_VERIFICATION_REQUIRED
        );
        assert_eq!(
            classify_mtop_response(200, &json!(["FAIL_SYS_USER_VALIDATE::x"])),
            CATEGORY_VERIFICATION_REQUIRED
        );
        assert_eq!(
            classify_mtop_response(403, &json!(["FAIL_SYS::x"])),
            CATEGORY_ACCESS_DENIED
        );
        assert_eq!(
            classify_mtop_response(429, &json!(["FAIL_SYS::x"])),
            CATEGORY_RATE_LIMITED
        );
        assert_eq!(
            classify_mtop_response(200, &json!(["FAIL_SYS_TRAFFIC_LIMIT::x"])),
            CATEGORY_RATE_LIMITED
        );
        assert_eq!(
            classify_mtop_response(200, &json!(["FAIL_SYS_API_NOT_FOUNDED::x"])),
            CATEGORY_API_NOT_FOUND
        );
        assert_eq!(
            classify_mtop_response(200, &json!(["FAIL_SYS_SESSION_EXPIRED::x"])),
            CATEGORY_LOGIN_REQUIRED
        );
        assert_eq!(
            classify_mtop_response(200, &json!(["FAIL_SYS_PROTOVER_MISSED::x"])),
            CATEGORY_SIGNING_OR_PROTOCOL
        );
        assert_eq!(
            classify_mtop_response(200, &json!(["FAIL_BIZ_SERVER_NEED_UPGRADE_MSG2::x"])),
            CATEGORY_APP_VERSION_REQUIRED
        );
        assert_eq!(
            classify_mtop_response(200, &json!(["FAIL_BIZ_ITEM_ERROR::x"])),
            CATEGORY_BUSINESS_ERROR
        );
        assert_eq!(
            classify_mtop_response(500, &json!(["SUCCESS::x"])),
            CATEGORY_HTTP_STATUS_ERROR
        );
        assert_eq!(classify_mtop_response(200, &json!([])), CATEGORY_UNKNOWN);
        assert_eq!(
            ret_code_of(&json!(["fail_sys_user_validate :: x"])),
            "FAIL_SYS_USER_VALIDATE"
        );
    }
}
