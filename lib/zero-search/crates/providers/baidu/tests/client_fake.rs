//! Port of Python `test_client_mock.py`, `test_errors.py`,
//! `test_pagination_control.py` and `test_integration_offline.py`.
//!
//! `FakeFetcher` plays the role of Python's FakeSession + the real
//! HttpTransport policy: the router returns a response (or a typed
//! transport exception), and the fetcher then applies the transport
//! checks — foreign-host redirect rejection, body cap, 403/429 ->
//! access_restricted, >= 400 -> http_error — before the client's own
//! validation / parsing / aggregation pipeline runs for real.

use baidu_search::client::{BaiduClient, Fetcher, TransportResponse};
use baidu_search::errors::*;
use baidu_search::urls;
use baidu_search::BaiduError;
use std::collections::HashMap;
use std::sync::Mutex;

type Router = Box<dyn Fn(&str) -> Result<TransportResponse, BaiduError> + Send + Sync>;

pub struct FakeFetcher {
    router: Router,
    pub requested_urls: Mutex<Vec<String>>,
    max_bytes: usize,
}

impl FakeFetcher {
    fn new(router: Router) -> Self {
        FakeFetcher {
            router,
            requested_urls: Mutex::new(Vec::new()),
            max_bytes: 8_000_000,
        }
    }
    fn with_max_bytes(mut self, n: usize) -> Self {
        self.max_bytes = n;
        self
    }
    fn requested(&self) -> Vec<String> {
        self.requested_urls.lock().unwrap().clone()
    }
}

impl Fetcher for FakeFetcher {
    async fn fetch(&self, url: &str) -> Result<TransportResponse, BaiduError> {
        self.requested_urls.lock().unwrap().push(url.to_string());
        let resp = (self.router)(url)?;
        // Transport policy (mirrors HttpTransport.fetch order).
        let final_host = urls::host_of(Some(&resp.final_url));
        if resp.redirect_count > 0 && !urls::is_baidu_host(final_host.as_deref()) {
            return Err(BaiduError::new(
                KIND_REDIRECT_ERROR,
                format!(
                    "redirected to unexpected host: {}",
                    final_host.as_deref().unwrap_or("")
                ),
            )
            .with_url(resp.final_url.clone())
            .with_status(resp.status_code));
        }
        if resp.body.len() > self.max_bytes {
            return Err(BaiduError::new(
                KIND_RESPONSE_TOO_LARGE,
                format!("response body exceeded {} bytes", self.max_bytes),
            )
            .with_url(resp.final_url.clone())
            .with_status(resp.status_code));
        }
        if resp.status_code == 403 || resp.status_code == 429 {
            return Err(BaiduError::new(
                KIND_ACCESS_RESTRICTED,
                format!("access restricted (HTTP {})", resp.status_code),
            )
            .with_url(resp.final_url.clone())
            .with_status(resp.status_code));
        }
        if resp.status_code >= 400 {
            return Err(BaiduError::new(
                KIND_HTTP_ERROR,
                format!("HTTP error {}", resp.status_code),
            )
            .with_url(resp.final_url.clone())
            .with_status(resp.status_code));
        }
        Ok(resp)
    }
}

fn resp(body: Vec<u8>, url: &str) -> TransportResponse {
    TransportResponse::new(200, url.to_string(), body)
}

fn fixture(name: &str) -> Vec<u8> {
    match name {
        "tianqi_p1.html" => include_bytes!("fixtures/tianqi_p1.html").to_vec(),
        "tianqi_pn10.html" => include_bytes!("fixtures/tianqi_pn10.html").to_vec(),
        "tianqi_pn20.html" => include_bytes!("fixtures/tianqi_pn20.html").to_vec(),
        "sapporo.html" => include_bytes!("fixtures/sapporo.html").to_vec(),
        "weather_en.html" => include_bytes!("fixtures/weather_en.html").to_vec(),
        _ => panic!("unknown fixture {}", name),
    }
}

fn tianqi_map() -> HashMap<i64, Vec<u8>> {
    HashMap::from([
        (0, fixture("tianqi_p1.html")),
        (10, fixture("tianqi_pn10.html")),
        (20, fixture("tianqi_pn20.html")),
    ])
}

fn pn_router(map: HashMap<i64, Vec<u8>>) -> Router {
    Box::new(move |url: &str| {
        let pn = urls::pn_of(Some(url)).unwrap_or(0);
        match map.get(&pn) {
            Some(body) => Ok(resp(body.clone(), url)),
            None => Ok(TransportResponse::new(
                404,
                url.to_string(),
                b"not found".to_vec(),
            )),
        }
    })
}

fn tianqi_client() -> BaiduClient<FakeFetcher> {
    BaiduClient::new(FakeFetcher::new(pn_router(tianqi_map())))
}

fn fixed_client(outcome: Result<TransportResponse, BaiduError>) -> BaiduClient<FakeFetcher> {
    BaiduClient::new(FakeFetcher::new(Box::new(
        move |url: &str| match &outcome {
            Ok(r) => {
                let mut r = r.clone();
                if r.final_url.is_empty() {
                    r.final_url = url.to_string();
                }
                Ok(r)
            }
            Err(e) => Err(e.clone()),
        },
    )))
}

// ------------------------------------------------------------ pipeline

#[tokio::test]
async fn one_page_search() {
    let client = tianqi_client();
    let result = client.search("天气", 1).await.unwrap();
    assert_eq!(result.pages_fetched, 1);
    assert_eq!(result.results.len(), 5);
    assert_eq!(result.cards.len(), 4);
    assert!(result.ads.is_empty());
    assert!(result.unknown_elements.is_empty());
    assert_eq!(
        result.to_value()["diagnostics"]["errors"],
        serde_json::json!([])
    );
    assert_eq!(client.fetcher().requested().len(), 1);
}

#[tokio::test]
async fn two_page_search() {
    let client = tianqi_client();
    let result = client.search("天气", 2).await.unwrap();
    assert_eq!(result.pages_fetched, 2);
    assert_eq!(result.results.len(), 5 + 6);
    let pns: Vec<i64> = result.pages.iter().map(|p| p.pn.unwrap()).collect();
    assert_eq!(pns, [0, 10]);
}

#[tokio::test]
async fn three_page_search_and_natural_end() {
    let client = tianqi_client();
    let result = client.search("天气", 5).await.unwrap();
    assert_eq!(result.pages_fetched, 3);
    assert_eq!(result.results.len(), 5 + 6 + 8);
    let pns: Vec<i64> = result.pages.iter().map(|p| p.pn.unwrap()).collect();
    assert_eq!(pns, [0, 10, 20]);
    assert_eq!(result.pages.last().unwrap().next_url, None);
    assert_eq!(client.fetcher().requested().len(), 3);
}

#[tokio::test]
async fn next_url_passed_through_verbatim() {
    let page1 = baidu_search::parse_search_results_bytes(
        &fixture("tianqi_p1.html"),
        Some("天气"),
        Some("https://m.baidu.com/s?word=%E5%A4%A9%E6%B0%94"),
    );
    let client = tianqi_client();
    client.search("天气", 2).await.unwrap();
    let requested = client.fetcher().requested();
    assert_eq!(requested[1], page1.page.next_url.unwrap());
    assert!(requested[1].contains("ant_ct="));
}

#[tokio::test]
async fn search_page_single_api() {
    let client = tianqi_client();
    let single = client.search_page("天气", None).await.unwrap();
    assert_eq!(single.page.query.as_deref(), Some("天气"));
    assert_eq!(single.page.results.len(), 5);
    assert!(single.page.page.next_url.is_some());
    assert_eq!(single.status_code, Some(200));
    let data = single.to_value();
    assert_eq!(data["fetch"]["status_code"], 200);
}

#[tokio::test]
async fn japanese_query() {
    let client = BaiduClient::new(FakeFetcher::new(pn_router(HashMap::from([(
        0,
        fixture("sapporo.html"),
    )]))));
    let result = client.search("札幌 天気", 1).await.unwrap();
    assert_eq!(result.pages_fetched, 1);
    assert_eq!(result.results.len(), 7);
    assert!(result
        .results
        .iter()
        .any(|r| r["title"].as_str().unwrap_or("").contains("札幌")));
}

#[tokio::test]
async fn english_query() {
    let client = BaiduClient::new(FakeFetcher::new(pn_router(HashMap::from([(
        0,
        fixture("weather_en.html"),
    )]))));
    let result = client.search("weather", 1).await.unwrap();
    assert_eq!(result.pages_fetched, 1);
    assert_eq!(result.results.len(), 3);
    assert_eq!(result.cards.len(), 6);
}

#[tokio::test]
async fn global_rank_is_new_field_and_parser_ranks_preserved() {
    let client = tianqi_client();
    let result = client.search("天气", 2).await.unwrap();
    let ranks: Vec<i64> = result
        .results
        .iter()
        .map(|r| r["global_organic_rank"].as_i64().unwrap())
        .collect();
    assert_eq!(ranks, (1..=result.results.len() as i64).collect::<Vec<_>>());
    let page2: Vec<_> = result
        .results
        .iter()
        .filter(|r| r["page_pn"] == 10)
        .collect();
    assert_eq!(page2[0]["organic_rank"], 1);
    assert_eq!(page2[0]["global_organic_rank"], 6);
}

#[tokio::test]
async fn json_serializable_and_readable_unicode() {
    let client = tianqi_client();
    let result = client.search("天气", 3).await.unwrap();
    let data = result.to_value();
    let text = serde_json::to_string(&data).unwrap();
    assert!(text.contains("天气"));
    let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(parsed, data);
    assert_eq!(parsed["query"], "天气");
    assert_eq!(parsed["pages_requested"], 3);
    assert_eq!(parsed["pages_fetched"], 3);
    for key in [
        "results",
        "ads",
        "cards",
        "related_searches",
        "unknown_elements",
        "pages",
    ] {
        assert!(parsed.get(key).is_some(), "missing {}", key);
    }
    let diag = parsed["diagnostics"].as_object().unwrap();
    for key in ["duplicates", "duplicate_items", "errors", "warnings"] {
        assert!(diag.contains_key(key), "missing diagnostics.{}", key);
    }
}

#[tokio::test]
async fn max_pages_clamped_with_warning() {
    let client = tianqi_client().with_max_pages_limit(2);
    let result = client.search("天气", 9).await.unwrap();
    assert_eq!(result.pages_requested, 9);
    assert_eq!(result.pages_fetched, 2);
    assert!(result.warnings.iter().any(|w| w.contains("clamped")));
}

#[tokio::test]
async fn waf_title_warning_not_page_rejection() {
    let client = tianqi_client();
    let result = client.search("天气", 1).await.unwrap();
    assert_eq!(result.pages_fetched, 1);
    assert!(result
        .warnings
        .iter()
        .any(|w| w.starts_with("waf_title_in_results")));
}

#[tokio::test]
async fn full_pipeline_three_pages_metrics() {
    let client = tianqi_client();
    let result = client.search("天气", 3).await.unwrap();
    let data = result.to_value();
    let roundtrip: serde_json::Value =
        serde_json::from_str(&serde_json::to_string(&data).unwrap()).unwrap();
    assert_eq!(roundtrip, data);
    assert_eq!(data["pages_fetched"], 3);
    assert_eq!(data["results"].as_array().unwrap().len(), 19);
    assert_eq!(data["cards"].as_array().unwrap().len(), 8);
    assert_eq!(data["ads"], serde_json::json!([]));
    assert_eq!(data["unknown_elements"].as_array().unwrap().len(), 0);
    assert_eq!(data["diagnostics"]["duplicates"], 0);
    assert_eq!(data["diagnostics"]["errors"], serde_json::json!([]));

    let results = data["results"].as_array().unwrap();
    let with = |key: &str| {
        results
            .iter()
            .filter(|r| r[key].as_str().map(|s| !s.is_empty()).unwrap_or(false))
            .count()
    };
    assert_eq!(with("title"), 19, "title rate must be 1.0");
    assert_eq!(with("url"), 19, "url rate must be 1.0");
    assert_eq!(with("snippet"), 15);
    let pages = data["pages"].as_array().unwrap();
    let pns: Vec<i64> = pages.iter().map(|p| p["pn"].as_i64().unwrap()).collect();
    assert_eq!(pns, [0, 10, 20]);
    assert_eq!(pages[0]["next_url"], pages[1]["source_url"]);
    assert_eq!(pages[1]["next_url"], pages[2]["source_url"]);
    assert_eq!(pages[2]["next_url"], serde_json::Value::Null);
}

// ------------------------------------------------------------ errors

#[tokio::test]
async fn http_403_is_access_restricted_and_stops() {
    let client = fixed_client(Ok(TransportResponse::new(
        403,
        String::new(),
        b"forbidden".to_vec(),
    )));
    let err = client.search_page("天气", None).await.unwrap_err();
    assert_eq!(err.kind, KIND_ACCESS_RESTRICTED);
    let result = client.search("天气", 3).await.unwrap();
    assert_eq!(result.pages_fetched, 0);
    let errs = result.to_value()["diagnostics"]["errors"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(errs[0]["kind"], "access_restricted");
    assert_eq!(errs[0]["status_code"], 403);
}

#[tokio::test]
async fn http_429_is_access_restricted() {
    let client = fixed_client(Ok(TransportResponse::new(
        429,
        String::new(),
        b"slow down".to_vec(),
    )));
    let err = client.search_page("天气", None).await.unwrap_err();
    assert_eq!(err.kind, KIND_ACCESS_RESTRICTED);
}

#[tokio::test]
async fn http_500_is_http_error() {
    let client = fixed_client(Ok(TransportResponse::new(
        500,
        String::new(),
        b"oops".to_vec(),
    )));
    let err = client.search_page("天气", None).await.unwrap_err();
    assert_eq!(err.kind, KIND_HTTP_ERROR);
    assert_eq!(err.status_code, Some(500));
}

#[tokio::test]
async fn timeout_dns_connection_tls_kinds() {
    for kind in [
        KIND_TIMEOUT,
        KIND_DNS_ERROR,
        KIND_CONNECTION_ERROR,
        KIND_TLS_ERROR,
    ] {
        let client = fixed_client(Err(
            BaiduError::new(kind, "fake transport failure").with_url("https://m.baidu.com/s")
        ));
        let err = client.search_page("天气", None).await.unwrap_err();
        assert_eq!(err.kind, kind);
    }
    // search() records the timeout in diagnostics and keeps 0 pages.
    let client = fixed_client(Err(BaiduError::new(KIND_TIMEOUT, "read timed out")));
    let result = client.search("天气", 2).await.unwrap();
    assert_eq!(
        result.to_value()["diagnostics"]["errors"][0]["kind"],
        "timeout"
    );
}

#[tokio::test]
async fn response_too_large() {
    let fetcher = FakeFetcher::new(pn_router(tianqi_map())).with_max_bytes(1024);
    let client = BaiduClient::new(fetcher);
    let err = client.search_page("天气", None).await.unwrap_err();
    assert_eq!(err.kind, KIND_RESPONSE_TOO_LARGE);
}

#[tokio::test]
async fn not_search_result_html() {
    let client = fixed_client(Ok(resp(
        b"<html><body><p>just a page</p></body></html>".to_vec(),
        "",
    )));
    let err = client.search_page("天气", None).await.unwrap_err();
    assert_eq!(err.kind, KIND_NOT_SEARCH_RESULT);
}

#[tokio::test]
async fn captcha_page_is_distinguished_from_results() {
    let body = "<html><body><div>安全验证</div><script src='https://wappass.baidu.com/static/captcha.js'></script></body></html>"
        .as_bytes()
        .to_vec();
    let client = fixed_client(Ok(resp(body, "")));
    let err = client.search_page("天气", None).await.unwrap_err();
    assert_eq!(err.kind, KIND_CAPTCHA);
    let result = client.search("天气", 1).await.unwrap();
    assert_eq!(
        result.to_value()["diagnostics"]["errors"][0]["kind"],
        "captcha_or_verification"
    );
}

#[tokio::test]
async fn empty_results_page_is_data_not_exception() {
    let client = fixed_client(Ok(resp(
        b"<html><body><div id=\"results\"></div></body></html>".to_vec(),
        "",
    )));
    let single = client
        .search_page("存在しないクエリxyz", None)
        .await
        .unwrap();
    assert!(single.page.results.is_empty());
    let result = client.search("存在しないクエリxyz", 1).await.unwrap();
    assert_eq!(result.pages_fetched, 1);
    assert!(result.results.is_empty());
    assert!(result.warnings.iter().any(|w| w == "empty_results"));
}

// ------------------------------------------------------------ pagination control

fn page_html(next_href: Option<&str>) -> Vec<u8> {
    let nav = match next_href {
        Some(h) => format!(
            "<div class=\"new-pagenav\"><a class=\"new-nextpage\" href=\"{}\">下一页</a></div>",
            h
        ),
        None => String::new(),
    };
    format!(
        "<html><body><div id=\"results\"></div>{}</body></html>",
        nav
    )
    .into_bytes()
}

#[tokio::test]
async fn cycle_detection() {
    let page_b_url = "https://m.baidu.com/s?word=x&pn=10";
    let router_state = std::sync::Arc::new(Mutex::new(None::<String>));
    let state2 = router_state.clone();
    let fetcher = FakeFetcher::new(Box::new(move |url: &str| {
        let mut first = state2.lock().unwrap();
        if first.is_none() {
            *first = Some(url.to_string());
            return Ok(resp(page_html(Some(page_b_url)), url));
        }
        if url == page_b_url {
            let first_url = first.as_ref().unwrap().clone();
            return Ok(resp(page_html(Some(&first_url)), url));
        }
        Ok(TransportResponse::new(
            404,
            url.to_string(),
            b"missing".to_vec(),
        ))
    }));
    let client = BaiduClient::new(fetcher);
    let result = client.search("x", 5).await.unwrap();
    assert_eq!(result.pages_fetched, 2);
    let data = result.to_value();
    let kinds: Vec<&str> = data["diagnostics"]["errors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["pagination_cycle"]);
    assert_eq!(client.fetcher().requested().len(), 2);
    let _ = router_state;
}

#[tokio::test]
async fn invalid_next_url_external_host() {
    let fetcher = FakeFetcher::new(Box::new(|url: &str| {
        Ok(resp(
            page_html(Some("https://evil.example.com/s?pn=10")),
            url,
        ))
    }));
    let client = BaiduClient::new(fetcher);
    let result = client.search("x", 3).await.unwrap();
    assert_eq!(result.pages_fetched, 1);
    let data = result.to_value();
    let kinds: Vec<&str> = data["diagnostics"]["errors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["invalid_next_url"]);
    assert_eq!(client.fetcher().requested().len(), 1);
}

#[tokio::test]
async fn invalid_next_url_scheme() {
    let fetcher = FakeFetcher::new(Box::new(|url: &str| {
        Ok(resp(page_html(Some("javascript:void(0)")), url))
    }));
    let client = BaiduClient::new(fetcher);
    let result = client.search("x", 3).await.unwrap();
    let data = result.to_value();
    let kinds: Vec<&str> = data["diagnostics"]["errors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["invalid_next_url"]);
    assert_eq!(client.fetcher().requested().len(), 1);
}

#[tokio::test]
async fn mid_pagination_http_error_keeps_fetched_pages() {
    let map = tianqi_map();
    let fetcher = FakeFetcher::new(Box::new(move |url: &str| {
        let pn = urls::pn_of(Some(url)).unwrap_or(0);
        if pn == 10 {
            return Ok(TransportResponse::new(
                500,
                url.to_string(),
                b"server error".to_vec(),
            ));
        }
        Ok(resp(map[&pn].clone(), url))
    }));
    let client = BaiduClient::new(fetcher);
    let result = client.search("天气", 3).await.unwrap();
    assert_eq!(result.pages_fetched, 1);
    assert_eq!(result.results.len(), 5);
    let errs = result.to_value()["diagnostics"]["errors"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(errs.len(), 1);
    assert_eq!(errs[0]["kind"], "http_error");
    assert_eq!(errs[0]["status_code"], 500);
}

#[tokio::test]
async fn redirect_to_foreign_host_rejected() {
    let fetcher = FakeFetcher::new(Box::new(|_url: &str| {
        let mut r = TransportResponse::new(
            200,
            "https://phishing.example.net/landing".to_string(),
            b"<html></html>".to_vec(),
        );
        r.redirect_count = 1;
        Ok(r)
    }));
    let client = BaiduClient::new(fetcher);
    let err = client.search_page("天气", None).await.unwrap_err();
    assert_eq!(err.kind, KIND_REDIRECT_ERROR);
}
