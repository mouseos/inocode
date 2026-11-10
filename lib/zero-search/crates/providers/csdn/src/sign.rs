//! CSDN app gateway (Alibaba Cloud API Gateway style) request signing.
//!
//! Reproduced from the Android app (net.csdn.csdnplus 6.5.7):
//! - `GateWaySignUtil` (obfuscated class `wi1`) builds the string-to-sign.
//! - The OkHttp interceptor in `CSDNRetrofit` (obfuscated class `kw$a`)
//!   applies it to every request whose URL starts with the gateway host
//!   `https://app-gw.csdn.net`, using the app key/secret compiled into
//!   the app itself.
//! - Common (non-signed) headers come from `gl0.j`.

use base64::Engine;
use hmac::{Hmac, Mac};
use sha2::Sha256;

pub const APP_KEY: &str = "203789067";
pub const APP_SECRET: &str = "0u94vkvsewic9kkgsp1r3nuq3ir0lv3n";
pub const GATEWAY_HOST: &str = "https://app-gw.csdn.net";

/// The app version the analyzed build reports in its version headers
/// (`version`, `c_appVersion`, `X-App-Version` in `gl0.j`).
pub const APP_VERSION: &str = "6.5.7";

type HmacSha256 = Hmac<Sha256>;

/// Canonical resource, as in `GateWaySignUtil.a`:
/// the URL-decoded path, then `?` plus parameters sorted by name
/// (query parameters and form fields merged), rendered as `name=value`,
/// or as a bare `name` when the value is empty.
pub fn canonical_resource(path: &str, params: &[(String, String)]) -> String {
    let mut out = String::from(path);
    if !params.is_empty() {
        let mut sorted: Vec<&(String, String)> = params.iter().collect();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));
        out.push('?');
        let mut first = true;
        for (name, value) in sorted {
            if !first {
                out.push('&');
            }
            first = false;
            out.push_str(name);
            if !value.is_empty() {
                out.push('=');
                out.push_str(value);
            }
        }
    }
    out
}

/// String-to-sign, as in `GateWaySignUtil.b`:
///
/// ```text
/// {METHOD}\n
/// {Accept}\n
/// {content-md5}\n        (empty for JSON bodies)
/// {content-type}\n
/// {date}\n               (empty)
/// X-Ca-Key:{key}\n
/// X-Ca-Nonce:{nonce}\n
/// X-Ca-Timestamp:{timestamp}\n
/// {canonical resource}
/// ```
pub fn string_to_sign(
    method: &str,
    accept: &str,
    content_type: &str,
    nonce: &str,
    timestamp: &str,
    canonical: &str,
) -> String {
    let mut s = String::new();
    s.push_str(&method.to_uppercase());
    s.push('\n');
    s.push_str(accept);
    s.push('\n');
    // content-md5: the app never sets it for these calls.
    s.push('\n');
    s.push_str(content_type);
    s.push('\n');
    // date: the app never sets it.
    s.push('\n');
    s.push_str("X-Ca-Key:");
    s.push_str(APP_KEY);
    s.push('\n');
    s.push_str("X-Ca-Nonce:");
    s.push_str(nonce);
    s.push('\n');
    s.push_str("X-Ca-Timestamp:");
    s.push_str(timestamp);
    s.push('\n');
    s.push_str(canonical);
    s
}

/// `X-Ca-Signature` = Base64(HMAC-SHA256(string-to-sign, app secret)).
pub fn signature(string_to_sign: &str) -> String {
    let mut mac =
        HmacSha256::new_from_slice(APP_SECRET.as_bytes()).expect("HMAC accepts any key length");
    mac.update(string_to_sign.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes())
}

/// The header set the app sends on gateway requests.
///
/// `gl0.j` contributes the common headers (platform, versions, app id,
/// device id, oaid, theme); the interceptor then adds the X-Ca set.
/// Login-only headers (Authorization, JWT-TOKEN, UserToken,
/// X-Access-Token) are absent for a guest session, matching the app
/// used while logged out.
pub struct GatewayHeaders {
    pub headers: Vec<(String, String)>,
}

pub fn build_headers(
    method: &str,
    path: &str,
    params: &[(String, String)],
    device_id: &str,
) -> Vec<(String, String)> {
    let nonce = uuid::Uuid::new_v4().to_string();
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis().to_string())
        .unwrap_or_else(|_| "0".to_string());
    let canonical = canonical_resource(path, params);
    let sts = string_to_sign(
        method,
        "application/json",
        "application/json; charset=UTF-8",
        &nonce,
        &timestamp,
        &canonical,
    );
    let sig = signature(&sts);

    let mut headers: Vec<(String, String)> = vec![
        ("platform".into(), "android".into()),
        ("version".into(), APP_VERSION.into()),
        ("c_appVersion".into(), APP_VERSION.into()),
        ("X-App-Version".into(), APP_VERSION.into()),
        ("X-App-ID".into(), "CSDN-APP".into()),
        ("X-OS".into(), "Android".into()),
        ("X-App-Theme".into(), "day".into()),
        ("X-Device-ID".into(), device_id.to_string()),
        ("oaid".into(), String::new()),
        ("X-Ca-Key".into(), APP_KEY.into()),
        ("Accept".into(), "application/json".into()),
        (
            "content-type".into(),
            "application/json; charset=UTF-8".into(),
        ),
        ("X-Ca-Nonce".into(), nonce),
        ("X-Ca-Timestamp".into(), timestamp),
        (
            "X-Ca-Signature-Headers".into(),
            "X-Ca-Timestamp,X-Ca-Key,X-Ca-Nonce".into(),
        ),
        ("X-Ca-Signature".into(), sig),
    ];
    // `requestID` in the app is a hash of the header map's string form
    // (a trace id; the gateway does not recompute it). We mirror the
    // shape with an MD5 hex of the Java-map-style rendering.
    let map_string = headers
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(", ");
    let map_string = format!("{{{map_string}}}");
    headers.push(("requestID".into(), md5_hex(map_string.as_bytes())));
    headers
}

fn md5_hex(data: &[u8]) -> String {
    use md5::{Digest, Md5};
    let mut hasher = Md5::new();
    hasher.update(data);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(32);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_sorts_params_and_bares_empty_values() {
        let c = canonical_resource(
            "/blog/phoenix/app/v1/get-article-basic-info",
            &[
                ("skin".to_string(), String::new()),
                ("articleId".to_string(), "12345".to_string()),
            ],
        );
        assert_eq!(
            c,
            "/blog/phoenix/app/v1/get-article-basic-info?articleId=12345&skin"
        );
    }

    #[test]
    fn canonical_without_params_is_plain_path() {
        let c = canonical_resource(
            "/silkroad-api/api/v2/assemble/list/pub/channel/app_search_result_v6",
            &[],
        );
        assert_eq!(
            c,
            "/silkroad-api/api/v2/assemble/list/pub/channel/app_search_result_v6"
        );
    }

    #[test]
    fn string_to_sign_layout() {
        let sts = string_to_sign(
            "POST",
            "application/json",
            "application/json; charset=UTF-8",
            "00000000-0000-0000-0000-000000000000",
            "1700000000000",
            "/silkroad-api/api/v2/assemble/list/pub/channel/app_search_result_v6",
        );
        let expected = "POST\napplication/json\n\napplication/json; charset=UTF-8\n\nX-Ca-Key:203789067\nX-Ca-Nonce:00000000-0000-0000-0000-000000000000\nX-Ca-Timestamp:1700000000000\n/silkroad-api/api/v2/assemble/list/pub/channel/app_search_result_v6";
        assert_eq!(sts, expected);
        // Signature is a 44-char Base64 SHA-256 HMAC.
        let sig = signature(&sts);
        assert_eq!(sig.len(), 44);
    }
}
