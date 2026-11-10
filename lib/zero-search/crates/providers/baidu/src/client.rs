//! Public client API — faithful port of Python `baidu_search.client`,
//! `baidu_search.transport`, `baidu_search.session` and
//! `baidu_search.pagination` behaviour:
//!
//! * one GET per page through a cookie-keeping session (reqwest jar),
//!   UA + `Accept: text/html,*/*`, 25 s default timeout;
//! * minimum 1 s interval between requests, **no retries ever**;
//! * streamed body capped at 8 MB (`response_too_large`);
//! * 403/429 -> `access_restricted`, >= 400 -> `http_error`,
//!   timeouts / DNS / connection / TLS classified from the error
//!   source chain, redirects leaving `*.baidu.com` rejected;
//! * response sanity checks beyond the status code (verification
//!   page -> `captcha_or_verification`, no `#results`/`.c-result`
//!   -> `not_search_result`, zero entries -> `empty_results` warning,
//!   WAF titles -> warning);
//! * pagination follows the server-issued `next_url` verbatim, with
//!   validation and exact-string cycle detection.
//!
//! The fetcher is a trait so tests can substitute a fake transport
//! (the Python suite's FakeSession equivalent).

use crate::errors::*;
use crate::models::{aggregate, AggregatedSearch, PageRecord, SinglePageResult};
use crate::pagination::{validate_next_url, SeenUrls};
use crate::parser;
use crate::search_url;
use crate::urls;
use std::collections::HashMap;
use std::future::Future;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

pub const DEFAULT_TIMEOUT_SECS: f64 = 25.0;
pub const DEFAULT_MAX_PAGES_LIMIT: i64 = 10;
pub const DEFAULT_MIN_INTERVAL_SECS: f64 = 1.0;
pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 8_000_000;

const VERIFICATION_MARKERS: &[&str] = &["wappass.baidu.com", "wappass", "安全验证"];
const WAF_TITLE_MARKERS: &[&str] = &["应用防火墙", "WEB 应用防火墙"];
const DNS_MARKERS: &[&str] = &[
    "Name or service not known",
    "Temporary failure in name resolution",
    "Failed to resolve",
    "NameResolutionError",
    "getaddrinfo failed",
    "nodename nor servname",
];

/// A raw transport response as returned by a [`Fetcher`].
#[derive(Debug, Clone)]
pub struct TransportResponse {
    pub status_code: i64,
    pub final_url: String,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
    pub redirect_count: i64,
}

impl TransportResponse {
    /// Create a response with empty headers and zero redirects.
    pub fn new(status_code: i64, final_url: String, body: Vec<u8>) -> Self {
        TransportResponse {
            status_code,
            final_url,
            headers: HashMap::new(),
            body,
            redirect_count: 0,
        }
    }
}

/// Transport abstraction (Python: the requests.Session subset used
/// by HttpTransport). Implementations apply status/error policy.
pub trait Fetcher: Send + Sync {
    fn fetch(
        &self,
        url: &str,
    ) -> impl Future<Output = Result<TransportResponse, BaiduError>> + Send;
}

/// Classify an error from its source-chain text. Mirrors the Python
/// transport's exception-type checks: TLS first (requests' SSLError
/// is checked before ConnectionError), then timeout, redirects,
/// then DNS-vs-connection by marker text, else generic transport.
pub fn classify_error_chain(
    chain_text: &str,
    is_timeout: bool,
    is_redirect: bool,
    url: &str,
    display: &str,
) -> BaiduError {
    let lower = chain_text.to_lowercase();
    if lower.contains("certificate")
        || lower.contains("ssl")
        || lower.contains("tls")
        || lower.contains("rustls")
    {
        return BaiduError::new(KIND_TLS_ERROR, format!("TLS error: {}", display))
            .with_url(url.to_string());
    }
    if is_timeout {
        return BaiduError::new(KIND_TIMEOUT, format!("request timed out: {}", display))
            .with_url(url.to_string());
    }
    if is_redirect {
        return BaiduError::new(
            KIND_REDIRECT_ERROR,
            format!("too many redirects: {}", display),
        )
        .with_url(url.to_string());
    }
    if DNS_MARKERS.iter().any(|m| chain_text.contains(m)) {
        return BaiduError::new(
            KIND_DNS_ERROR,
            format!("DNS resolution failed: {}", display),
        )
        .with_url(url.to_string());
    }
    if lower.contains("dns") || lower.contains("failed to lookup") {
        return BaiduError::new(
            KIND_DNS_ERROR,
            format!("DNS resolution failed: {}", display),
        )
        .with_url(url.to_string());
    }
    BaiduError::new(
        KIND_CONNECTION_ERROR,
        format!("connection failed: {}", display),
    )
    .with_url(url.to_string())
}

fn error_chain_text(e: &reqwest::Error) -> String {
    let mut parts = vec![e.to_string()];
    let mut src: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(e);
    while let Some(s) = src {
        parts.push(s.to_string());
        src = s.source();
    }
    parts.join(": ")
}

/// Real HTTP fetcher (Python HttpTransport + SearchSession):
/// reqwest client with cookie store, representative UA, Accept
/// header, mandatory timeout, request interval, body cap.
pub struct ReqwestFetcher {
    client: reqwest::Client,
    max_bytes: usize,
    min_interval: f64,
    last_request_at: Mutex<Option<Instant>>,
}

impl ReqwestFetcher {
    /// Create a reqwest fetcher with an explicit timeout, body cap and minimum request interval (seconds).
    pub fn new(timeout_secs: f64, max_bytes: usize, min_interval: f64) -> Self {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::ACCEPT,
            reqwest::header::HeaderValue::from_static("text/html,*/*"),
        );
        let client = reqwest::Client::builder()
            .cookie_store(true)
            .user_agent(crate::urlbuild::USER_AGENT)
            .default_headers(headers)
            .timeout(Duration::from_secs_f64(timeout_secs))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        ReqwestFetcher {
            client,
            max_bytes,
            min_interval,
            last_request_at: Mutex::new(None),
        }
    }

    async fn respect_interval(&self) {
        if self.min_interval <= 0.0 {
            return;
        }
        let wait = {
            let last = self.last_request_at.lock().await;
            match *last {
                Some(t) => {
                    let elapsed = t.elapsed().as_secs_f64();
                    let w = self.min_interval - elapsed;
                    if w > 0.0 {
                        w
                    } else {
                        0.0
                    }
                }
                None => 0.0,
            }
        };
        if wait > 0.0 {
            tokio::time::sleep(Duration::from_secs_f64(wait)).await;
        }
        *self.last_request_at.lock().await = Some(Instant::now());
    }
}

impl Default for ReqwestFetcher {
    fn default() -> Self {
        Self::new(
            DEFAULT_TIMEOUT_SECS,
            DEFAULT_MAX_RESPONSE_BYTES,
            DEFAULT_MIN_INTERVAL_SECS,
        )
    }
}

impl Fetcher for ReqwestFetcher {
    async fn fetch(&self, url: &str) -> Result<TransportResponse, BaiduError> {
        self.respect_interval().await;
        let mut resp = match self.client.get(url).send().await {
            Ok(r) => r,
            Err(e) => {
                let chain = error_chain_text(&e);
                let is_timeout = e.is_timeout();
                let is_redirect = e.is_redirect();
                // reqwest wraps DNS/connect failures as request
                // errors; classify from the chain text like Python.
                if e.is_connect() || e.is_request() || !chain.is_empty() {
                    return Err(classify_error_chain(
                        &chain,
                        is_timeout,
                        is_redirect,
                        url,
                        &e.to_string(),
                    ));
                }
                return Err(BaiduError::new(
                    KIND_TRANSPORT_ERROR,
                    format!("transport error: {}", e),
                )
                .with_url(url.to_string()));
            }
        };
        let status = resp.status().as_u16() as i64;
        let final_url = resp.url().to_string();
        let redirect_count: i64 = if final_url != url { 1 } else { 0 };
        let mut headers = HashMap::new();
        for (k, v) in resp.headers().iter() {
            headers.insert(k.to_string(), v.to_str().unwrap_or_default().to_string());
        }
        let final_host = urls::host_of(Some(&final_url));
        if redirect_count > 0 && !urls::is_baidu_host(final_host.as_deref()) {
            return Err(BaiduError::new(
                KIND_REDIRECT_ERROR,
                format!(
                    "redirected to unexpected host: {}",
                    final_host.as_deref().unwrap_or("")
                ),
            )
            .with_url(final_url.clone())
            .with_status(status));
        }
        let mut body: Vec<u8> = Vec::new();
        loop {
            match resp.chunk().await {
                Ok(Some(chunk)) => {
                    body.extend_from_slice(&chunk);
                    if body.len() > self.max_bytes {
                        return Err(BaiduError::new(
                            KIND_RESPONSE_TOO_LARGE,
                            format!("response body exceeded {} bytes", self.max_bytes),
                        )
                        .with_url(final_url.clone())
                        .with_status(status));
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    let chain = error_chain_text(&e);
                    return Err(classify_error_chain(
                        &chain,
                        e.is_timeout(),
                        e.is_redirect(),
                        url,
                        &e.to_string(),
                    ));
                }
            }
        }
        if status == 403 || status == 429 {
            return Err(BaiduError::new(
                KIND_ACCESS_RESTRICTED,
                format!("access restricted (HTTP {})", status),
            )
            .with_url(final_url)
            .with_status(status));
        }
        if status >= 400 {
            return Err(
                BaiduError::new(KIND_HTTP_ERROR, format!("HTTP error {}", status))
                    .with_url(final_url)
                    .with_status(status),
            );
        }
        Ok(TransportResponse {
            status_code: status,
            final_url,
            headers,
            body,
            redirect_count,
        })
    }
}

/// End-to-end Baidu (APK-flow) search client.
pub struct BaiduClient<F: Fetcher> {
    fetcher: F,
    pub max_pages_limit: i64,
}

impl<F: Fetcher> BaiduClient<F> {
    /// Create a client over a custom [`Fetcher`] (fake, cached or live transport).
    pub fn new(fetcher: F) -> Self {
        BaiduClient {
            fetcher,
            max_pages_limit: DEFAULT_MAX_PAGES_LIMIT,
        }
    }
    /// Override the maximum number of pages [`BaiduClient::search`] will follow.
    pub fn with_max_pages_limit(mut self, limit: i64) -> Self {
        self.max_pages_limit = limit;
        self
    }
    /// Borrow the underlying fetcher.
    pub fn fetcher(&self) -> &F {
        &self.fetcher
    }

    /// Fetch and parse exactly one results page. With
    /// `page_url = None` the first-page URL is built from `query`.
    pub async fn search_page(
        &self,
        query: &str,
        page_url: Option<&str>,
    ) -> Result<SinglePageResult, BaiduError> {
        let url = match page_url {
            Some(u) => u.to_string(),
            None => search_url::build_initial_search_url(query, None),
        };
        let response = self.fetcher.fetch(&url).await?;
        self.parse_response(response, query, &url)
    }

    /// Fetch exactly one page addressed by its 1-based page number
    /// (audit W-06). The URL is built with the `pn` offset scheme
    /// the server-issued pagination URLs use; see
    /// [`crate::urlbuild::build_search_url_for_page`].
    pub async fn search_page_at(
        &self,
        query: &str,
        page: i64,
    ) -> Result<SinglePageResult, BaiduError> {
        let url =
            crate::urlbuild::build_search_url_for_page(query, page, crate::search_url::now_ms())
                .ok_or_else(|| BaiduError::new(KIND_BAIDU_SEARCH_ERROR, "page must be >= 1"))?;
        self.search_page(query, Some(&url)).await
    }

    /// Fetch one vertical (tab) search page — `video`, `image`,
    /// `realtime` or `wenda` (audit W-02). Vertical pages are
    /// served from the app's `sf/vsearch` URL template; they do
    /// not necessarily carry the main page's `#results` container,
    /// so the main-page gate in [`Self::parse_response`] is not
    /// applied, but verification pages are still rejected.
    ///
    /// Live status (2026-11-08): a direct anonymous GET of the
    /// plain template URL is redirected to the Baidu home page,
    /// and the bundled-config variant (`atn=page&fr=tab`) returns
    /// the `sf` app shell (~1.5 MB) whose results are rendered
    /// client-side — no server-rendered result markup is served
    /// either way, so this currently yields an empty page with an
    /// `empty_results` warning rather than items. The URL
    /// construction itself is exact per the app template.
    pub async fn search_vertical(
        &self,
        vertical: &str,
        query: &str,
    ) -> Result<SinglePageResult, BaiduError> {
        let url = crate::urlbuild::build_vertical_search_url(vertical, query).ok_or_else(|| {
            BaiduError::new(
                KIND_BAIDU_SEARCH_ERROR,
                format!("unknown vertical '{}'", vertical),
            )
        })?;
        let response = self.fetcher.fetch(&url).await?;
        let text = String::from_utf8_lossy(&response.body).into_owned();
        if VERIFICATION_MARKERS
            .iter()
            .any(|m| text.contains(m) || response.final_url.contains(m))
        {
            return Err(BaiduError::new(
                KIND_CAPTCHA,
                "server returned a verification page instead of search results",
            )
            .with_url(response.final_url.clone())
            .with_status(response.status_code));
        }
        let page = parser::parse_search_results_bytes(
            &response.body,
            Some(query),
            Some(&response.final_url),
        );
        let total_entries =
            page.results.len() + page.ads.len() + page.cards.len() + page.unknown_elements.len();
        let mut warnings: Vec<String> = Vec::new();
        if total_entries == 0 {
            warnings.push("empty_results".to_string());
        }
        Ok(SinglePageResult {
            query: Some(query.to_string()),
            source_url: url,
            final_url: Some(response.final_url),
            status_code: Some(response.status_code),
            page,
            warnings,
        })
    }

    /// Python `BaiduSearchClient._parse_response`.
    pub fn parse_response(
        &self,
        response: TransportResponse,
        query: &str,
        requested_url: &str,
    ) -> Result<SinglePageResult, BaiduError> {
        let text = String::from_utf8_lossy(&response.body).into_owned();
        if !text.contains("id=\"results\"")
            && VERIFICATION_MARKERS
                .iter()
                .any(|m| text.contains(m) || response.final_url.contains(m))
        {
            return Err(BaiduError::new(
                KIND_CAPTCHA,
                "server returned a verification page instead of search results",
            )
            .with_url(response.final_url.clone())
            .with_status(response.status_code));
        }
        let page = parser::parse_search_results_bytes(
            &response.body,
            Some(query),
            Some(&response.final_url),
        );
        let warnings = page.diagnostics_warnings();
        if page.c_result_count() == 0 && warnings.iter().any(|w| w.contains("no #results")) {
            return Err(BaiduError::new(
                KIND_NOT_SEARCH_RESULT,
                "HTTP 200 response is not a search-result page \
                 (no #results container, no .c-result elements)",
            )
            .with_url(response.final_url.clone())
            .with_status(response.status_code));
        }
        let mut client_warnings: Vec<String> = Vec::new();
        let total_entries =
            page.results.len() + page.ads.len() + page.cards.len() + page.unknown_elements.len();
        if total_entries == 0 {
            client_warnings.push("empty_results".to_string());
        }
        let waf_count = page
            .results
            .iter()
            .chain(page.ads.iter())
            .filter(|r| {
                r.title
                    .as_ref()
                    .map(|t| WAF_TITLE_MARKERS.iter().any(|m| t.contains(m)))
                    .unwrap_or(false)
            })
            .count();
        if waf_count > 0 {
            client_warnings.push(format!(
                "waf_title_in_results: {} result title(s) are WAF page titles stored by the server; the page itself is a valid results page",
                waf_count
            ));
        }
        Ok(SinglePageResult {
            query: Some(query.to_string()),
            source_url: requested_url.to_string(),
            final_url: Some(response.final_url),
            status_code: Some(response.status_code),
            page,
            warnings: client_warnings,
        })
    }

    /// Search `query`, following next-page URLs up to `max_pages`.
    /// Per-page failures are recorded in diagnostics and pagination
    /// stops; pages already fetched are kept.
    pub async fn search(
        &self,
        query: &str,
        max_pages: i64,
    ) -> Result<AggregatedSearch, BaiduError> {
        if max_pages < 1 {
            return Err(BaiduError::new(
                KIND_BAIDU_SEARCH_ERROR,
                "max_pages must be >= 1",
            ));
        }
        let requested_pages = max_pages;
        let mut warnings: Vec<String> = Vec::new();
        let mut max_pages = max_pages;
        if max_pages > self.max_pages_limit {
            warnings.push(format!(
                "max_pages={} clamped to limit {}",
                max_pages, self.max_pages_limit
            ));
            max_pages = self.max_pages_limit;
        }

        let mut singles: Vec<SinglePageResult> = Vec::new();
        let mut failed_pages: Vec<PageRecord> = Vec::new();
        let mut errors: Vec<serde_json::Value> = Vec::new();
        let mut seen = SeenUrls::new();

        let mut url: Option<String> = Some(search_url::build_initial_search_url(query, None));
        let mut fetch_index: i64 = 0;
        while let Some(current) = url.clone() {
            if (singles.len() + failed_pages.len()) as i64 >= max_pages {
                break;
            }
            fetch_index += 1;
            seen.add(&current);
            let single = match self.search_page(query, Some(&current)).await {
                Ok(s) => s,
                Err(exc) => {
                    let mut err = exc.to_value();
                    if let Some(obj) = err.as_object_mut() {
                        obj.insert(
                            "page_fetch_index".to_string(),
                            serde_json::json!(fetch_index),
                        );
                    }
                    errors.push(err.clone());
                    failed_pages.push(PageRecord {
                        page_number: None,
                        pn: None,
                        source_url: Some(current.clone()),
                        final_url: exc.url.clone(),
                        status_code: exc.status_code,
                        error: Some(err),
                        ..Default::default()
                    });
                    break;
                }
            };
            let next_url = single.page.page.next_url.clone();
            singles.push(single);
            let next_url = match next_url {
                Some(n) => n,
                None => break,
            };
            if (singles.len() + failed_pages.len()) as i64 >= max_pages {
                break;
            }
            let check = validate_next_url(&next_url).and_then(|_| seen.check(&next_url));
            if let Err(exc) = check {
                let mut err = exc.to_value();
                if let Some(obj) = err.as_object_mut() {
                    obj.insert(
                        "page_fetch_index".to_string(),
                        serde_json::json!(fetch_index + 1),
                    );
                }
                errors.push(err);
                break;
            }
            url = Some(next_url);
        }

        Ok(aggregate(
            query,
            requested_pages,
            &singles,
            &errors,
            &warnings,
            &failed_pages,
        ))
    }
}

impl BaiduClient<ReqwestFetcher> {
    /// Live client with the Python defaults (25 s timeout, 1 s interval, 8 MB body cap).
    pub fn new_live() -> Self {
        BaiduClient::new(ReqwestFetcher::default())
    }
    /// Live client with explicit timeout, page limit, interval and body-cap settings.
    pub fn new_live_with(
        timeout_secs: f64,
        max_pages_limit: i64,
        min_interval_secs: f64,
        max_response_bytes: usize,
    ) -> Self {
        BaiduClient {
            fetcher: ReqwestFetcher::new(timeout_secs, max_response_bytes, min_interval_secs),
            max_pages_limit,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_dns_vs_connection_vs_tls() {
        let dns = classify_error_chain(
            "HTTPSConnectionPool: Failed to resolve 'm.baidu.com' ([Errno -2] Name or service not known)",
            false,
            false,
            "https://m.baidu.com/s",
            "Failed to resolve",
        );
        assert_eq!(dns.kind, KIND_DNS_ERROR);
        let conn = classify_error_chain(
            "Connection reset by peer",
            false,
            false,
            "https://m.baidu.com/s",
            "Connection reset by peer",
        );
        assert_eq!(conn.kind, KIND_CONNECTION_ERROR);
        let tls = classify_error_chain(
            "certificate verify failed: unable to get local issuer certificate",
            false,
            false,
            "https://m.baidu.com/s",
            "certificate verify failed",
        );
        assert_eq!(tls.kind, KIND_TLS_ERROR);
        let timeout = classify_error_chain(
            "read timed out",
            true,
            false,
            "https://m.baidu.com/s",
            "read timed out",
        );
        assert_eq!(timeout.kind, KIND_TIMEOUT);
    }
}
