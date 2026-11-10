//! Stable adapter over `urlbuild` — port of Python
//! `baidu_search.search_url`. `now_ms` exists only so tests can pin
//! the clock; production callers leave it `None`.

use crate::urlbuild;

/// The User-Agent string used by the live client (APK-derived).
pub fn user_agent() -> String {
    urlbuild::USER_AGENT.to_string()
}

/// Current time in milliseconds since the Unix epoch.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Build the first-page search URL for `query`
/// (delegates 1:1 to `urlbuild::build_search_url`).
pub fn build_initial_search_url(query: &str, now: Option<i64>) -> String {
    let ms = now.unwrap_or_else(now_ms);
    urlbuild::build_search_url(query, ms)
}
