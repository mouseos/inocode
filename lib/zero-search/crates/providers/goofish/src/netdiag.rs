//! Network diagnostics for the live path (issue #2).
//!
//! Two jobs:
//!
//! - **Error observability.** A failed `reqwest` send used to surface
//!   only `e.to_string()` ("error sending request for url (...)"),
//!   discarding the `source()` chain that carries the actual cause
//!   (connect refused / TLS handshake / proxy tunnel / DNS). This
//!   module keeps the whole chain and classifies the failure into a
//!   fine-grained category (`connect_error` / `tls_error` /
//!   `proxy_error` / `connect_timeout` / `timeout` /
//!   `transport_error`), plus whether it is a *connection
//!   establishment* failure — the only kind that may be retried,
//!   because no response (in fact no request bytes) was received.
//!
//! - **Proxy diagnostics.** This deployment can only reach the
//!   gateway through the environment proxy (direct egress resolves
//!   to fake IPs and TLS never completes), so callers need to confirm
//!   that an env proxy was detected. The diagnostic reports scheme,
//!   host and port only — userinfo (user name / password) from the
//!   proxy URL is *never* included in any output.

use std::error::Error;

// ---------------------------------------------------------------------------
// Error chains
// ---------------------------------------------------------------------------

/// The full `source()` chain of an error, outermost first.
pub fn error_chain(err: &(dyn Error + 'static)) -> Vec<String> {
    let mut out = Vec::new();
    let mut current: Option<&(dyn Error + 'static)> = Some(err);
    while let Some(e) = current {
        out.push(e.to_string());
        current = e.source();
    }
    out
}

/// The chain rendered as one line: `outer: caused by: inner: ...`.
pub fn format_error_chain(err: &(dyn Error + 'static)) -> String {
    error_chain(err).join(": caused by: ")
}

// ---------------------------------------------------------------------------
// Transport error classification
// ---------------------------------------------------------------------------

pub const CATEGORY_CONNECT_ERROR: &str = "connect_error";
pub const CATEGORY_TLS_ERROR: &str = "tls_error";
pub const CATEGORY_PROXY_ERROR: &str = "proxy_error";
pub const CATEGORY_CONNECT_TIMEOUT: &str = "connect_timeout";
pub const CATEGORY_TIMEOUT: &str = "timeout";
pub const CATEGORY_TRANSPORT_ERROR: &str = "transport_error";

/// Classification outcome for a failed send.
#[derive(Debug, Clone, PartialEq)]
pub struct TransportErrorInfo {
    pub category: &'static str,
    /// True when the failure happened while establishing the
    /// connection (TCP connect, proxy CONNECT tunnel, or TLS
    /// handshake) — i.e. before any request was sent and before any
    /// response could exist. Only these may be retried.
    pub connect_phase: bool,
    /// The full source chain, outermost first.
    pub chain: Vec<String>,
}

/// Pure classification core (unit-testable without a live reqwest
/// error): reqwest's `is_timeout` / `is_connect` flags plus the
/// lower-cased concatenation of the whole source chain.
pub fn classify_from_parts(
    is_timeout: bool,
    is_connect: bool,
    chain_text_lower: &str,
) -> (&'static str, bool) {
    let is_proxy = chain_text_lower.contains("proxy");
    let is_tls = chain_text_lower.contains("tls")
        || chain_text_lower.contains("ssl")
        || chain_text_lower.contains("certificate")
        || chain_text_lower.contains("handshake")
        // OpenSSL's verdict when the peer is not speaking TLS at all
        // (e.g. a fake-IP direct connect): a handshake failure.
        || chain_text_lower.contains("wrong version number");
    if is_proxy {
        return (CATEGORY_PROXY_ERROR, true);
    }
    if is_tls {
        return (CATEGORY_TLS_ERROR, true);
    }
    if is_timeout && is_connect {
        return (CATEGORY_CONNECT_TIMEOUT, true);
    }
    if is_timeout {
        // A timeout after the connection existed (request possibly
        // sent, response possibly in flight): never retried.
        return (CATEGORY_TIMEOUT, false);
    }
    if is_connect {
        return (CATEGORY_CONNECT_ERROR, true);
    }
    (CATEGORY_TRANSPORT_ERROR, false)
}

pub fn classify_reqwest_error(err: &reqwest::Error) -> TransportErrorInfo {
    let chain = error_chain(err);
    let text = chain.join(" ").to_lowercase();
    let (category, connect_phase) = classify_from_parts(err.is_timeout(), err.is_connect(), &text);
    TransportErrorInfo {
        category,
        connect_phase,
        chain,
    }
}

// ---------------------------------------------------------------------------
// Proxy diagnostics (host only — never credentials)
// ---------------------------------------------------------------------------

/// What was found in the environment proxy variables. Carries scheme,
/// host and port only; any userinfo in the proxy URL is dropped at
/// parse time and cannot leak through this type.
#[derive(Debug, Clone, PartialEq)]
pub struct ProxyDiagnostic {
    pub detected: bool,
    pub scheme: Option<String>,
    pub host: Option<String>,
    pub port: Option<u16>,
    /// Name of the environment variable the proxy came from.
    pub source_var: Option<String>,
}

impl ProxyDiagnostic {
    pub fn none() -> Self {
        Self {
            detected: false,
            scheme: None,
            host: None,
            port: None,
            source_var: None,
        }
    }

    /// One-line summary safe for logs and error payloads: scheme,
    /// host and port only, never userinfo.
    pub fn summary(&self) -> String {
        if !self.detected {
            return "no env proxy detected".to_string();
        }
        let scheme = self.scheme.as_deref().unwrap_or("http");
        let host = self.host.as_deref().unwrap_or("?");
        match self.port {
            Some(port) => format!("env proxy detected: {scheme}://{host}:{port}"),
            None => format!("env proxy detected: {scheme}://{host}"),
        }
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "detected": self.detected,
            "scheme": self.scheme,
            "host": self.host,
            "port": self.port,
            "source_var": self.source_var,
            "summary": self.summary(),
        })
    }
}

/// Parse a proxy URL into (scheme, host, port), dropping userinfo.
/// Handles `scheme://[user[:password]@]host[:port][/...]`, bare
/// `host:port`, and bracketed IPv6 hosts. Returns `None` when no host
/// can be found.
pub fn parse_proxy_authority(value: &str) -> Option<(String, String, Option<u16>)> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let (scheme, rest) = match value.split_once("://") {
        Some((s, r)) => (s.to_lowercase(), r),
        None => ("http".to_string(), value),
    };
    let authority = rest.split('/').next().unwrap_or("");
    // Strip userinfo: everything up to the last '@'.
    let hostport = match authority.rsplit_once('@') {
        Some((_, hp)) => hp,
        None => authority,
    };
    if hostport.is_empty() {
        return None;
    }
    if let Some(stripped) = hostport.strip_prefix('[') {
        // Bracketed IPv6: [::1] or [::1]:8080
        let (host, tail) = stripped.split_once(']')?;
        let port = tail.strip_prefix(':').and_then(|p| p.parse::<u16>().ok());
        return Some((scheme, host.to_string(), port));
    }
    match hostport.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => {
            Some((scheme, host.to_string(), port.parse::<u16>().ok()))
        }
        _ => Some((scheme, hostport.to_string(), None)),
    }
}

/// Proxy variables in the precedence order reqwest's system-proxy
/// handling consults them for an HTTPS target (lowercase first, as
/// the conventional CGI-safe spelling), then the generic fallbacks.
const PROXY_VARS: [&str; 6] = [
    "https_proxy",
    "HTTPS_PROXY",
    "all_proxy",
    "ALL_PROXY",
    "http_proxy",
    "HTTP_PROXY",
];

/// Pure detection over provided (name, value) pairs.
pub fn detect_proxy_in(vars: &[(String, String)]) -> ProxyDiagnostic {
    for name in PROXY_VARS {
        for (k, v) in vars {
            if k == name {
                if let Some((scheme, host, port)) = parse_proxy_authority(v) {
                    return ProxyDiagnostic {
                        detected: true,
                        scheme: Some(scheme),
                        host: Some(host),
                        port,
                        source_var: Some(name.to_string()),
                    };
                }
            }
        }
    }
    ProxyDiagnostic::none()
}

/// Detect the environment proxy of the current process.
pub fn proxy_diagnostic() -> ProxyDiagnostic {
    let vars: Vec<(String, String)> = PROXY_VARS
        .iter()
        .filter_map(|name| std::env::var(name).ok().map(|v| (name.to_string(), v)))
        .collect();
    detect_proxy_in(&vars)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt;

    #[derive(Debug)]
    struct FakeError {
        message: &'static str,
        source: Option<Box<FakeError>>,
    }

    impl fmt::Display for FakeError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "{}", self.message)
        }
    }

    impl Error for FakeError {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            self.source.as_deref().map(|e| e as &(dyn Error + 'static))
        }
    }

    #[test]
    fn error_chain_walks_all_sources() {
        let err = FakeError {
            message: "error sending request for url",
            source: Some(Box::new(FakeError {
                message: "client error (Connect)",
                source: Some(Box::new(FakeError {
                    message: "tcp connect error: Connection refused (os error 111)",
                    source: None,
                })),
            })),
        };
        let chain = error_chain(&err);
        assert_eq!(chain.len(), 3);
        assert_eq!(
            chain[2],
            "tcp connect error: Connection refused (os error 111)"
        );
        let formatted = format_error_chain(&err);
        assert!(formatted.starts_with("error sending request for url: caused by: "));
        assert!(formatted.contains("Connection refused"));
    }

    #[test]
    fn error_chain_single_error() {
        let err = FakeError {
            message: "only",
            source: None,
        };
        assert_eq!(error_chain(&err), vec!["only".to_string()]);
        assert_eq!(format_error_chain(&err), "only");
    }

    #[test]
    fn classify_table() {
        // Plain connect failure.
        assert_eq!(
            classify_from_parts(
                false,
                true,
                "error sending request client error (connect) tcp connect error"
            ),
            (CATEGORY_CONNECT_ERROR, true)
        );
        // TLS handshake failure during connect.
        assert_eq!(
            classify_from_parts(false, true, "client error (connect) tls handshake eof"),
            (CATEGORY_TLS_ERROR, true)
        );
        // Fake-IP direct connect: OpenSSL "wrong version number".
        assert_eq!(
            classify_from_parts(
                false,
                true,
                "client error (connect) error:0a00010b:ssl routines::wrong version number"
            ),
            (CATEGORY_TLS_ERROR, true)
        );
        // Certificate validation.
        assert_eq!(
            classify_from_parts(
                false,
                true,
                "client error (connect) certificate verify failed"
            ),
            (CATEGORY_TLS_ERROR, true)
        );
        // Proxy tunnel failure wins over the generic connect bucket.
        assert_eq!(
            classify_from_parts(
                false,
                true,
                "client error (connect) proxy server responded 407 proxy authentication required"
            ),
            (CATEGORY_PROXY_ERROR, true)
        );
        // Connect timeout: establishment phase, retryable.
        assert_eq!(
            classify_from_parts(true, true, "error sending request operation timed out"),
            (CATEGORY_CONNECT_TIMEOUT, true)
        );
        // Read timeout after connect: not retryable.
        assert_eq!(
            classify_from_parts(true, false, "error reading response operation timed out"),
            (CATEGORY_TIMEOUT, false)
        );
        // Anything else.
        assert_eq!(
            classify_from_parts(false, false, "error sending request for url"),
            (CATEGORY_TRANSPORT_ERROR, false)
        );
    }

    #[test]
    fn parse_proxy_authority_strips_userinfo() {
        let (scheme, host, port) =
            parse_proxy_authority("http://proxy-user:proxy-password@proxy.example.com:3128")
                .unwrap();
        assert_eq!(scheme, "http");
        assert_eq!(host, "proxy.example.com");
        assert_eq!(port, Some(3128));
        // The whole point: credentials cannot survive parsing.
        let diag = detect_proxy_in(&[(
            "https_proxy".to_string(),
            "http://proxy-user:proxy-password@proxy.example.com:3128".to_string(),
        )]);
        let summary = diag.summary();
        assert!(summary.contains("proxy.example.com:3128"), "{summary}");
        assert!(!summary.contains("proxy-user"), "{summary}");
        assert!(!summary.contains("proxy-password"), "{summary}");
        let json = diag.to_json().to_string();
        assert!(!json.contains("proxy-user"), "{json}");
        assert!(!json.contains("proxy-password"), "{json}");
    }

    #[test]
    fn parse_proxy_authority_variants() {
        assert_eq!(
            parse_proxy_authority("proxy.internal:8080"),
            Some(("http".to_string(), "proxy.internal".to_string(), Some(8080)))
        );
        assert_eq!(
            parse_proxy_authority("socks5://127.0.0.1:1080"),
            Some(("socks5".to_string(), "127.0.0.1".to_string(), Some(1080)))
        );
        assert_eq!(
            parse_proxy_authority("http://[::1]:3128/"),
            Some(("http".to_string(), "::1".to_string(), Some(3128)))
        );
        assert_eq!(
            parse_proxy_authority("https://proxy.example.com"),
            Some(("https".to_string(), "proxy.example.com".to_string(), None))
        );
        assert_eq!(parse_proxy_authority(""), None);
        assert_eq!(parse_proxy_authority("http://"), None);
    }

    #[test]
    fn detect_proxy_precedence_and_absence() {
        let vars = vec![
            (
                "HTTP_PROXY".to_string(),
                "http://upper.example:1000".to_string(),
            ),
            (
                "https_proxy".to_string(),
                "http://lower.example:2000".to_string(),
            ),
        ];
        let diag = detect_proxy_in(&vars);
        assert!(diag.detected);
        assert_eq!(diag.host.as_deref(), Some("lower.example"));
        assert_eq!(diag.source_var.as_deref(), Some("https_proxy"));

        let none = detect_proxy_in(&[("UNRELATED".to_string(), "http://x:1".to_string())]);
        assert!(!none.detected);
        assert_eq!(none.summary(), "no env proxy detected");

        // An unparseable value does not count as detected and does
        // not block a later usable variable.
        let vars = vec![
            ("https_proxy".to_string(), "http://".to_string()),
            (
                "HTTPS_PROXY".to_string(),
                "http://fallback.example:3128".to_string(),
            ),
        ];
        let diag = detect_proxy_in(&vars);
        assert_eq!(diag.host.as_deref(), Some("fallback.example"));
    }
}
