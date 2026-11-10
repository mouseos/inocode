//! Yahoo (US) image-search fetch helper for ZeroSearch MCP.
//!
//! Yahoo's image host (`images.search.yahoo.com`) answers plain HTTP
//! clients with HTTP 500 and an empty body — under every condition
//! tested (UA, cookies, Yahoo's own SERP-issued pivot URL, US and
//! Japanese egress) — while a real Firefox on the same connection
//! renders the results fine. The edge discriminates on the request
//! fingerprint below the application layer (TLS handshake / HTTP/2
//! behaviour), so the page is fetched here with a browser-grade
//! client (wreq with a Firefox emulation profile): same URL, same
//! server-rendered page, no browser driven.
//!
//! That client's TLS stack (BoringSSL) exports the same libssl /
//! libcrypto symbols as the OpenSSL the main server binary links
//! (via reqwest/native-tls and the Amazon provider's libcurl), and
//! the two cannot coexist in one binary — hence this separate
//! helper process, spawned per request by the Yahoo US provider,
//! mirroring how the Goofish provider already shells out to its
//! signing harness.
//!
//! Protocol (one request per process invocation):
//!   stdin:  one JSON object, `{"url": "https://…"}` (a single line)
//!   stdout: one JSON object on a single line —
//!           `{"ok": true, "status": 200, "html": "…"}` after a
//!           completed HTTP exchange (any status), or
//!           `{"ok": false, "error": "…"}` when the fetch itself
//!           failed before a response arrived.
//! Nothing else is written to stdout; diagnostics go to stderr.

use serde::{Deserialize, Serialize};
use std::io::Read;
use std::time::Duration;

/// Request the server sends on stdin.
#[derive(Debug, Deserialize)]
struct Request {
    url: String,
}

/// Response printed on stdout.
#[derive(Debug, Serialize)]
struct Response {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    html: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl Response {
    fn fetched(status: u16, html: String) -> Self {
        Self {
            ok: true,
            status: Some(status),
            html: Some(html),
            error: None,
        }
    }

    fn failed(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            status: None,
            html: None,
            error: Some(message.into()),
        }
    }
}

/// Parse the request line. Pure; unit-tested below.
fn parse_request(input: &str) -> Result<Request, String> {
    let line = input.lines().next().unwrap_or("").trim();
    if line.is_empty() {
        return Err("empty request".to_string());
    }
    let req: Request =
        serde_json::from_str(line).map_err(|e| format!("invalid request JSON: {e}"))?;
    if !req.url.starts_with("https://") {
        return Err(format!("refusing non-https url: {}", req.url));
    }
    Ok(req)
}

async fn fetch(url: &str) -> Response {
    let client = match wreq::Client::builder()
        .emulation(
            wreq_util::Emulation::builder()
                .profile(wreq_util::Profile::Firefox151)
                .build(),
        )
        .cookie_store(true)
        .timeout(Duration::from_secs(30))
        .build()
    {
        Ok(c) => c,
        Err(e) => return Response::failed(format!("failed to build HTTP client: {e}")),
    };
    let resp = match client.get(url).send().await {
        Ok(r) => r,
        Err(e) => return Response::failed(format!("request failed: {e}")),
    };
    let status = resp.status().as_u16();
    match resp.text().await {
        Ok(html) => Response::fetched(status, html),
        Err(e) => Response::failed(format!("failed to read response body: {e}")),
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let mut input = String::new();
    let response = match std::io::stdin().read_to_string(&mut input) {
        Ok(_) => match parse_request(&input) {
            Ok(req) => fetch(&req.url).await,
            Err(e) => Response::failed(e),
        },
        Err(e) => Response::failed(format!("failed to read stdin: {e}")),
    };
    // Single-line JSON on stdout is the whole protocol; a broken pipe
    // here means the parent went away, which needs no further action.
    println!(
        "{}",
        serde_json::to_string(&response).expect("response serialises")
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_well_formed_request() {
        let req =
            parse_request("{\"url\": \"https://images.search.yahoo.com/search/images?p=apple\"}\n")
                .unwrap();
        assert_eq!(
            req.url,
            "https://images.search.yahoo.com/search/images?p=apple"
        );
    }

    #[test]
    fn rejects_garbage_and_non_https() {
        assert!(parse_request("").is_err());
        assert!(parse_request("not json").is_err());
        assert!(parse_request("{\"url\": 42}").is_err());
        assert!(parse_request("{\"url\": \"http://example.com/\"}").is_err());
    }

    #[test]
    fn response_shapes_serialise_as_documented() {
        let ok = serde_json::to_value(Response::fetched(200, "<html/>".into())).unwrap();
        assert_eq!(ok["ok"], true);
        assert_eq!(ok["status"], 200);
        assert_eq!(ok["html"], "<html/>");
        assert!(ok.get("error").is_none());

        let err = serde_json::to_value(Response::failed("boom")).unwrap();
        assert_eq!(err["ok"], false);
        assert_eq!(err["error"], "boom");
        assert!(err.get("status").is_none());
        assert!(err.get("html").is_none());
    }
}
