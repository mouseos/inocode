use pretty_assertions::assert_eq;
use zero_search::EngineOutcome;
use zero_search::ErrorKind;
use zero_search::SearchItem;
use zero_search::WebEngine;

use super::*;

fn item(engine: WebEngine, n: usize, url: &str) -> SearchItem {
    SearchItem {
        engine,
        title: format!("{} title {n}", engine.id()),
        url: url.to_string(),
        snippet: Some(format!("snippet {n}")),
    }
}

fn ok(engine: WebEngine, items: Vec<SearchItem>) -> EngineOutcome {
    EngineOutcome {
        engine,
        ok: true,
        items,
        error: None,
        error_kind: None,
    }
}

fn failed(engine: WebEngine, kind: ErrorKind) -> EngineOutcome {
    EngineOutcome {
        engine,
        ok: false,
        items: Vec::new(),
        error: Some("boom".to_string()),
        error_kind: Some(kind),
    }
}

#[test]
fn results_alternate_between_engines_and_drop_duplicate_urls() {
    let outcomes = vec![
        ok(
            WebEngine::Bing,
            vec![
                item(WebEngine::Bing, 1, "https://a.example"),
                item(WebEngine::Bing, 2, "https://b.example"),
            ],
        ),
        ok(
            WebEngine::Baidu,
            vec![
                item(WebEngine::Baidu, 1, "https://a.example"),
                item(WebEngine::Baidu, 2, "https://c.example"),
            ],
        ),
    ];
    let urls: Vec<&str> = merge_items(&outcomes, 10)
        .iter()
        .map(|item| item.url.as_str())
        .collect();
    assert_eq!(
        urls,
        ["https://a.example", "https://b.example", "https://c.example"]
    );
    assert_eq!(merge_items(&outcomes, 2).len(), 2);
}

#[test]
fn output_lists_results_and_names_failed_engines_with_their_kind() {
    let outcomes = vec![
        ok(WebEngine::Bing, vec![item(WebEngine::Bing, 1, "https://a.example")]),
        failed(WebEngine::Yandex, ErrorKind::Captcha),
    ];
    let (text, any) = format_web_results("rust", &outcomes, 5);
    assert!(any);
    assert_eq!(
        text,
        "Web search results for \"rust\"\n1. bing title 1\n   https://a.example  [bing]\n   snippet 1\nEngines that failed: yandex (captcha)\n"
    );
}

#[test]
fn all_engines_failing_reports_no_results() {
    let outcomes = vec![
        failed(WebEngine::Bing, ErrorKind::RateLimited),
        failed(WebEngine::Baidu, ErrorKind::NetworkError),
    ];
    let (text, any) = format_web_results("rust", &outcomes, 5);
    assert!(!any);
    assert!(text.contains("No results."));
    assert!(text.contains("bing (rate_limited), baidu (network_error)"));
}

#[test]
fn long_output_is_cut_on_a_character_boundary() {
    let text = "検".repeat(MAX_OUTPUT_BYTES);
    let cut = truncate_output(text);
    assert!(cut.len() <= MAX_OUTPUT_BYTES);
    assert!(cut.ends_with("… [truncated]"));
}
