//! Fetching Yahoo pages through the helper process.
//!
//! Yahoo's edge refuses plain HTTP clients on some hosts (HTTP 500,
//! empty body) while answering real browsers — measured first on
//! the image host, and on `search.yahoo.com` itself in the Z12
//! diagnosis (both egresses tested, plain clients always 500, the
//! helper's browser-grade request always 200). Pages are therefore
//! fetched — image pages unconditionally, the other surfaces as a
//! fallback when the direct fetch fails — by
//! `zero-search-yahoo-us-image-helper`, a small companion binary
//! whose browser-grade HTTP client (wreq, Firefox emulation profile)
//! reproduces the browser request fingerprint. That client's TLS
//! stack (BoringSSL) cannot be linked into the main server binary
//! alongside OpenSSL, so it runs as a separate process — the same
//! shape the Goofish provider already uses for its signing harness.
//! The helper ships next to the server binary in every package; the
//! parsing of the page it returns stays here (see [`crate::image`]).
//!
//! Protocol (one request per process invocation): the helper reads
//! `{"url": "…"}` on stdin and prints one JSON line on stdout —
//! `{"ok": true, "status": …, "html": …}` after a completed HTTP
//! exchange, or `{"ok": false, "error": …}` when the fetch failed.

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// File name of the helper binary shipped next to the server.
pub const HELPER_NAME: &str = if cfg!(windows) {
    "zero-search-yahoo-us-image-helper.exe"
} else {
    "zero-search-yahoo-us-image-helper"
};

/// Optional environment override for the helper's location. Normal
/// installs never need it — the helper sits next to the server —
/// but it keeps development and packaging experiments configuration-
/// free for everyone else.
pub const HELPER_ENV: &str = "ZERO_SEARCH_YAHOO_US_IMAGE_HELPER";

/// Upper bound for one helper round trip. The helper's own client
/// times out at 30s; this leaves room for process start and a slow
/// first BoringSSL build-up on cold systems.
const HELPER_TIMEOUT: Duration = Duration::from_secs(60);

/// The helper's stdout line, decoded.
#[derive(Debug, Deserialize)]
struct HelperResponse {
    ok: bool,
    status: Option<u16>,
    html: Option<String>,
    error: Option<String>,
}

/// Encode the request line sent to the helper's stdin. Pure.
pub(crate) fn encode_request(url: &str) -> String {
    serde_json::json!({ "url": url }).to_string()
}

/// Decode the helper's stdout line into `(status, html)`. Pure.
fn decode_response(line: &str) -> Result<(u16, String)> {
    let line = line.trim();
    if line.is_empty() {
        return Err(anyhow!("image helper produced no output"));
    }
    let resp: HelperResponse = serde_json::from_str(line)
        .map_err(|e| anyhow!("image helper returned malformed output: {e}"))?;
    if !resp.ok {
        return Err(anyhow!(
            "image helper failed: {}",
            resp.error.unwrap_or_else(|| "unknown error".to_string())
        ));
    }
    match (resp.status, resp.html) {
        (Some(status), Some(html)) => Ok((status, html)),
        _ => Err(anyhow!("image helper response is missing status or html")),
    }
}

/// Locate the helper binary. Resolution order: the
/// [`HELPER_ENV`] override, then next to the running executable
/// (the shipped layout — the installer ZIP and the MCPB both place
/// it beside the server), then the executable's parent directory
/// (cargo puts test binaries in `deps/`, one level deeper), then
/// the workspace `target/` directories resolved from this crate's
/// manifest directory (independent of the process working
/// directory — `cargo test` runs providers from their crate root,
/// where the old working-directory-relative candidates never
/// matched), then the working-directory-relative `target/`
/// directories (development fallback).
pub fn helper_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var(HELPER_ENV) {
        let p = p.trim();
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join(HELPER_NAME));
            if let Some(parent) = dir.parent() {
                candidates.push(parent.join(HELPER_NAME));
            }
        }
    }
    // This crate lives at crates/providers/yahoo-us, so the
    // workspace root is three ancestors up from the manifest dir.
    if let Some(root) = PathBuf::from(env!("CARGO_MANIFEST_DIR")).ancestors().nth(3) {
        for profile in ["debug", "release"] {
            candidates.push(root.join("target").join(profile).join(HELPER_NAME));
            candidates.push(
                root.join("crates")
                    .join("helpers")
                    .join("yahoo-us-image")
                    .join("target")
                    .join(profile)
                    .join(HELPER_NAME),
            );
        }
    }
    for profile in ["debug", "release"] {
        candidates.push(PathBuf::from("target").join(profile).join(HELPER_NAME));
        // The helper is a standalone crate (outside the workspace),
        // so its builds land in its own target directory.
        candidates.push(
            PathBuf::from("crates")
                .join("helpers")
                .join("yahoo-us-image")
                .join("target")
                .join(profile)
                .join(HELPER_NAME),
        );
    }
    candidates.into_iter().find(|p| p.is_file())
}

/// Fetch one image-search page through the helper process and
/// return its HTML. HTTP error statuses are reported with the same
/// wording the rest of this crate's error mapping expects.
pub(crate) async fn fetch_image_html(url: &str) -> Result<String> {
    let (status, html) = run_helper(url).await?;
    if !(200..300).contains(&status) {
        return Err(anyhow!("yahoo us image search returned HTTP {status}"));
    }
    Ok(html)
}

/// Fetch one page through the helper process as the fallback for a
/// failed direct fetch (see [`crate::YahooUsSearchClient::get_html`]).
/// Same protocol as [`fetch_image_html`]; only the error wording
/// differs.
pub(crate) async fn fetch_page_html(url: &str) -> Result<String> {
    let (status, html) = run_helper(url).await?;
    if !(200..300).contains(&status) {
        return Err(anyhow!("yahoo us page fetch returned HTTP {status}"));
    }
    Ok(html)
}

/// Run the helper for one URL and return `(status, body)`.
async fn run_helper(url: &str) -> Result<(u16, String)> {
    let path = helper_path().ok_or_else(|| {
        anyhow!(
            "image helper not found (expected {HELPER_NAME} next to the server binary); \
             Yahoo US image search is unavailable in this installation"
        )
    })?;
    let mut child = Command::new(&path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("failed to start image helper {}", path.display()))?;
    if let Some(mut stdin) = child.stdin.take() {
        let mut line = encode_request(url);
        line.push('\n');
        // A helper that exits early makes this write fail; the
        // response decoding below reports the useful error.
        let _ = stdin.write_all(line.as_bytes()).await;
    }
    let output = tokio::time::timeout(HELPER_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| anyhow!("image helper timed out after {}s", HELPER_TIMEOUT.as_secs()))?
        .context("image helper process failed")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.lines().last().unwrap_or("");
    decode_response(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_is_one_json_object() {
        assert_eq!(
            encode_request("https://images.search.yahoo.com/search/images?p=apple"),
            "{\"url\":\"https://images.search.yahoo.com/search/images?p=apple\"}"
        );
    }

    #[test]
    fn decodes_a_successful_response() {
        let (status, html) =
            decode_response("{\"ok\":true,\"status\":200,\"html\":\"<html/>\"}").unwrap();
        assert_eq!(status, 200);
        assert_eq!(html, "<html/>");
    }

    #[test]
    fn decodes_an_error_response() {
        let err = decode_response("{\"ok\":false,\"error\":\"boom\"}").unwrap_err();
        assert!(err.to_string().contains("boom"), "{err}");
    }

    #[test]
    fn rejects_empty_and_malformed_output() {
        assert!(decode_response("").is_err());
        assert!(decode_response("not json").is_err());
        assert!(decode_response("{\"ok\":true}").is_err());
    }
}
