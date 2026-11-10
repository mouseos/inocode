//! Port of Python `test_synthetic.py`: ads, entities, edge cases.

use baidu_search::urls::{classify_click_url, pn_of, resolve};
use baidu_search::{parse_search_results, parse_search_results_bytes};

const PAGE_URL: &str = "https://m.baidu.com/s?word=test";

fn synthetic() -> Vec<u8> {
    include_bytes!("fixtures/synthetic_ad.html").to_vec()
}

#[test]
fn ad_separation_and_entities() {
    let page = parse_search_results_bytes(&synthetic(), Some("test"), Some(PAGE_URL));
    assert_eq!(page.results.len(), 2);
    assert_eq!(page.ads.len(), 1);
    assert_eq!(page.unknown_elements.len(), 1);
    assert_eq!(
        page.unknown_elements[0].tpl.as_deref(),
        Some("some_future_tpl")
    );

    let organic = &page.results[0];
    assert_eq!(organic.title.as_deref(), Some("Organic & Title"));
    assert_eq!(
        organic.snippet.as_deref(),
        Some("An organic snippet with emphasis kept.")
    );
    assert_eq!(organic.organic_rank, Some(1));

    let ad = &page.ads[0];
    assert_eq!(ad.title.as_deref(), Some("Sponsored Thing"));
    assert_eq!(ad.url.as_deref(), Some("https://ads.example.com/landing"));
    assert_eq!(ad.organic_rank, None);
    assert_eq!(page.results[1].organic_rank, Some(2));
}

#[test]
fn relative_url_resolution() {
    let page = parse_search_results_bytes(&synthetic(), Some("test"), Some(PAGE_URL));
    let relative = &page.results[1];
    assert_eq!(relative.title.as_deref(), Some("Relative Link Title"));
    assert_eq!(
        relative.click_url.as_deref(),
        Some("https://m.baidu.com/s?word=rel")
    );
    assert_eq!(
        resolve(Some("/s?word=x"), Some(PAGE_URL)).as_deref(),
        Some("https://m.baidu.com/s?word=x")
    );
    assert_eq!(resolve(None, Some(PAGE_URL)), None);
}

#[test]
fn click_url_classification() {
    assert_eq!(
        classify_click_url(Some("https://m.baidu.com/from=0/x/t=zbios/l=1/tc?a=1")).as_deref(),
        Some("baidu_redirect")
    );
    assert_eq!(
        classify_click_url(Some("baiduboxapp://v1/browser/open?url=x")).as_deref(),
        Some("baidu_app_scheme")
    );
    assert_eq!(
        classify_click_url(Some("https://baike.baidu.com/item/x")).as_deref(),
        Some("baidu_internal")
    );
    assert_eq!(
        classify_click_url(Some("https://example.com/")).as_deref(),
        Some("direct_external")
    );
    assert_eq!(classify_click_url(None), None);
    assert_eq!(pn_of(Some("https://m.baidu.com/s?pn=30&word=x")), Some(30));
    assert_eq!(pn_of(Some("https://m.baidu.com/s?word=x")), None);
}

#[test]
fn empty_and_broken_pages() {
    let empty = parse_search_results("<html><body></body></html>", Some("x"), None);
    assert!(empty.results.is_empty() && empty.ads.is_empty() && empty.cards.is_empty());
    assert_eq!(empty.page.next_url, None);
    assert!(empty
        .diagnostics_warnings()
        .iter()
        .any(|w| w.contains("c-result")));
    serde_json::to_string(&empty.to_value()).unwrap();

    let no_results_div = parse_search_results(
        "<html><body><div id=\"results\"></div></body></html>",
        None,
        None,
    );
    assert_eq!(no_results_div.c_result_count(), 0);
}

#[test]
fn missing_attributes_do_not_crash() {
    let html = "<html><body><div id=\"results\">\
        <div class=\"c-result result\" tpl=\"www_index\">\
        <h3>No attributes at all</h3></div>\
        <div class=\"c-result result\"><h3>No tpl</h3></div>\
        </div></body></html>";
    let page = parse_search_results(html, Some("x"), Some(PAGE_URL));
    assert_eq!(page.results.len(), 1);
    assert_eq!(
        page.results[0].title.as_deref(),
        Some("No attributes at all")
    );
    assert_eq!(page.results[0].display_position, None);
    assert_eq!(page.results[0].url, None);
    assert_eq!(page.unknown_elements.len(), 1);
    assert!(page.unknown_elements[0].reason.contains("tpl"));
    assert!(page
        .diagnostics_warnings()
        .iter()
        .any(|w| w.contains("order")));
    serde_json::to_string(&page.to_value()).unwrap();
}

#[test]
fn utf8_input_forms_equivalent() {
    let raw = synthetic();
    let as_str = parse_search_results(
        std::str::from_utf8(&raw).unwrap(),
        Some("test"),
        Some(PAGE_URL),
    );
    let as_bytes = parse_search_results_bytes(&raw, Some("test"), Some(PAGE_URL));
    assert_eq!(as_str.to_value(), as_bytes.to_value());
}
