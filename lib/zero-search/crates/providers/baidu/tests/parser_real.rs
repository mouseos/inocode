//! Port of Python `test_real_pages.py`: measured-page fixtures with
//! expectations fixed by manual DOM inspection of the original HTML.

use baidu_search::models::SearchPage;
use baidu_search::parse_search_results_bytes;

fn fixture(name: &str) -> &'static [u8] {
    match name {
        "tianqi_p1.html" => include_bytes!("fixtures/tianqi_p1.html"),
        "tianqi_pn10.html" => include_bytes!("fixtures/tianqi_pn10.html"),
        "tianqi_pn20.html" => include_bytes!("fixtures/tianqi_pn20.html"),
        "sapporo.html" => include_bytes!("fixtures/sapporo.html"),
        "weather_en.html" => include_bytes!("fixtures/weather_en.html"),
        _ => panic!("unknown fixture {}", name),
    }
}

fn page_url(name: &str) -> (&'static str, &'static str) {
    match name {
        "tianqi_p1.html" => ("天气", "https://m.baidu.com/s?word=%E5%A4%A9%E6%B0%94"),
        "tianqi_pn10.html" => (
            "天气",
            "https://m.baidu.com/s?word=%E5%A4%A9%E6%B0%94&pn=10",
        ),
        "tianqi_pn20.html" => (
            "天气",
            "https://m.baidu.com/s?word=%E5%A4%A9%E6%B0%94&pn=20",
        ),
        "sapporo.html" => (
            "札幌 天気",
            "https://m.baidu.com/s?word=%E6%9C%AD%E5%B9%8C%20%E5%A4%A9%E6%B0%94",
        ),
        "weather_en.html" => ("weather", "https://m.baidu.com/s?word=weather"),
        _ => panic!("unknown fixture {}", name),
    }
}

fn parse_fixture(name: &str) -> SearchPage {
    let (q, url) = page_url(name);
    parse_search_results_bytes(fixture(name), Some(q), Some(url))
}

#[test]
fn classification_counts() {
    // (name, organic, ads, cards, unknown, c_results)
    let cases = [
        ("tianqi_p1.html", 5, 0, 4, 0, 11),
        ("tianqi_pn10.html", 6, 0, 4, 0, 12),
        ("tianqi_pn20.html", 8, 0, 0, 0, 10),
        ("sapporo.html", 7, 0, 2, 0, 11),
        ("weather_en.html", 3, 0, 6, 0, 11),
    ];
    for (name, organic, ads, cards, unknown, c_results) in cases {
        let page = parse_fixture(name);
        assert_eq!(page.c_result_count(), c_results, "{} c_result_count", name);
        assert_eq!(page.results.len(), organic, "{} organic", name);
        assert_eq!(page.ads.len(), ads, "{} ads", name);
        assert_eq!(page.cards.len(), cards, "{} cards", name);
        assert_eq!(page.unknown_elements.len(), unknown, "{} unknown", name);
    }
}

#[test]
fn card_kinds() {
    let pg = parse_fixture("tianqi_p1.html");
    let kinds: Vec<&str> = pg
        .cards
        .iter()
        .map(|c| c.card_kind.as_deref().unwrap())
        .collect();
    assert_eq!(
        kinds,
        ["weather", "knowledge_graph", "note", "official_account"]
    );
    let pg = parse_fixture("tianqi_pn10.html");
    let kinds: Vec<&str> = pg
        .cards
        .iter()
        .map(|c| c.card_kind.as_deref().unwrap())
        .collect();
    assert_eq!(kinds, ["baike", "baike", "forum", "live"]);
    let pg = parse_fixture("weather_en.html");
    let kinds: Vec<&str> = pg
        .cards
        .iter()
        .map(|c| c.card_kind.as_deref().unwrap())
        .collect();
    assert_eq!(
        kinds,
        [
            "dictionary",
            "knowledge_graph",
            "video",
            "note",
            "video",
            "baike"
        ]
    );
    let pg = parse_fixture("sapporo.html");
    let kinds: Vec<&str> = pg
        .cards
        .iter()
        .map(|c| c.card_kind.as_deref().unwrap())
        .collect();
    assert_eq!(kinds, ["ai_answer", "note"]);
}

#[test]
fn tianqi_first_result() {
    let page = parse_fixture("tianqi_p1.html");
    let r = &page.results[0];
    assert_eq!(r.organic_rank, Some(1));
    assert_eq!(r.display_position, Some(3));
    assert_eq!(r.title.as_deref(), Some("WEB 应用防火墙"));
    assert_eq!(r.url.as_deref(), Some("http://weather.cma.cn/"));
    assert_eq!(r.url_source.as_deref(), Some("data_log.mu"));
    assert_eq!(r.domain.as_deref(), Some("weather.cma.cn"));
    assert_eq!(r.display_url.as_deref(), Some("weather.cma.cn"));
    assert_eq!(r.site_name.as_deref(), Some("中国气象局"));
    assert!(
        r.snippet
            .as_deref()
            .unwrap()
            .starts_with("当前正在对访问请求进行人机识别检测"),
        "snippet: {:?}",
        r.snippet
    );
    let sitelink_texts: Vec<&str> = r.sitelinks.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(sitelink_texts, ["南京", "黑牛城", "南宁", "沙坪坝"]);
    assert!(r.is_baidu_redirect);
    assert_eq!(r.click_url_kind.as_deref(), Some("baidu_redirect"));
}

#[test]
fn tianqi_titles_and_dates() {
    let page = parse_fixture("tianqi_p1.html");
    let results = &page.results;
    assert!(results[1]
        .title
        .as_deref()
        .unwrap()
        .contains("2026天马参赛指南来了"));
    assert_eq!(results[1].date.as_deref(), Some("10月25日"));
    assert_eq!(results[1].date_source.as_deref(), Some("source_sdata"));
    assert_eq!(results[2].title.as_deref(), Some("天气网"));
    assert_eq!(
        results[2].url.as_deref(),
        Some("https://www.weather.com.cn/?u3wwy=67351")
    );
    assert!(results[3]
        .title
        .as_deref()
        .unwrap()
        .starts_with("【大自然天气预报15天"));
    assert!(results[4]
        .title
        .as_deref()
        .unwrap()
        .contains("将减弱为温带气旋"));
}

#[test]
fn snippet_date_prefix_separated() {
    let page = parse_fixture("tianqi_pn10.html");
    let r = page
        .results
        .iter()
        .find(|x| x.display_position == Some(7))
        .unwrap();
    assert_eq!(
        r.title.as_deref(),
        Some("...海洋天气公报-中华人民共和国交通运输部")
    );
    assert_eq!(r.site_name.as_deref(), Some("交通运输部"));
    assert_eq!(r.date.as_deref(), Some("10月25日"));
    assert!(r
        .snippet
        .as_deref()
        .unwrap()
        .starts_with("一、近海海域天气实况"));
    assert!(!r.snippet.as_deref().unwrap().starts_with("4小时前"));
}

#[test]
fn www_specific_has_no_snippet() {
    let page = parse_fixture("tianqi_pn20.html");
    let specific: Vec<_> = page
        .results
        .iter()
        .filter(|r| r.tpl.as_deref() == Some("www_specific"))
        .collect();
    assert_eq!(specific.len(), 3);
    assert!(specific.iter().all(|r| r.snippet.is_none()));
    assert!(specific
        .iter()
        .all(|r| r.title.as_deref().map(|t| !t.is_empty()).unwrap_or(false)));
}

#[test]
fn display_order_and_ranks() {
    let page = parse_fixture("tianqi_pn20.html");
    let positions: Vec<i64> = page
        .results
        .iter()
        .map(|r| r.display_position.unwrap())
        .collect();
    assert_eq!(positions, [1, 2, 3, 4, 5, 7, 8, 9]);
    let ranks: Vec<i64> = page
        .results
        .iter()
        .map(|r| r.organic_rank.unwrap())
        .collect();
    assert_eq!(ranks, (1..=8).collect::<Vec<_>>());
}

#[test]
fn japanese_page() {
    let page = parse_fixture("sapporo.html");
    let titles: Vec<String> = page
        .results
        .iter()
        .map(|r| r.title.clone().unwrap_or_default())
        .collect();
    assert!(
        titles[0].starts_with("【札幌市天气预报15天"),
        "{}",
        titles[0]
    );
    assert_eq!(page.results[0].domain.as_deref(), Some("uc.weather.com.cn"));
    assert!(titles
        .iter()
        .any(|t| t.contains("札幌市, 北海道の月間天気予報 - weather.com")));
    assert_eq!(page.results[1].date.as_deref(), Some("10月20日"));
}

#[test]
fn ai_answer_card() {
    let page = parse_fixture("sapporo.html");
    let card = page
        .cards
        .iter()
        .find(|c| c.card_kind.as_deref() == Some("ai_answer"))
        .unwrap();
    assert!(card.ai_generated_marker);
    assert_eq!(card.url, None);
    assert_eq!(card.text, None);
}

#[test]
fn english_page() {
    let page = parse_fixture("weather_en.html");
    let r = &page.results[0];
    assert_eq!(r.title.as_deref(), Some("weather - 搜索 词典"));
    assert_eq!(r.url.as_deref(), Some("https://cn.bing.com/dict/weather"));
    let dictionary = page
        .cards
        .iter()
        .find(|c| c.card_kind.as_deref() == Some("dictionary"))
        .unwrap();
    assert_eq!(dictionary.title.as_deref(), Some("weather - 百度翻译"));
    assert!(dictionary
        .url
        .as_deref()
        .unwrap()
        .starts_with("https://fanyi.baidu.com/"));
    let kg = page
        .cards
        .iter()
        .find(|c| c.card_kind.as_deref() == Some("knowledge_graph"))
        .unwrap();
    assert!(kg
        .text
        .as_deref()
        .unwrap()
        .starts_with("Weather是英语多词性词汇"));
    let video = page
        .cards
        .iter()
        .find(|c| c.card_kind.as_deref() == Some("video"))
        .unwrap();
    assert_eq!(
        video.url.as_deref(),
        Some("https://www.bilibili.com/video/BV1VX4y1y7WV")
    );
}

#[test]
fn knowledge_graph_text() {
    let page = parse_fixture("tianqi_p1.html");
    let kg = page
        .cards
        .iter()
        .find(|c| c.card_kind.as_deref() == Some("knowledge_graph"))
        .unwrap();
    assert!(kg.text.as_deref().unwrap().starts_with("天气，汉语词语"));
    assert_eq!(
        kg.url.as_deref(),
        Some("https://baike.baidu.com/item/%E5%A4%A9%E6%B0%94/332535")
    );
}

#[test]
fn pagination_chain() {
    let p1 = parse_fixture("tianqi_p1.html").page;
    assert_eq!((p1.pn, p1.page_number, p1.is_first_page), (0, 1, true));
    assert_eq!(p1.next_pn, Some(10));
    assert_eq!(p1.previous_url, None);
    assert!(p1.next_url.as_deref().unwrap().contains("pn=10"));
    assert!(p1
        .next_url
        .as_deref()
        .unwrap()
        .contains("word=%E5%A4%A9%E6%B0%94"));

    let p2 = parse_fixture("tianqi_pn10.html").page;
    assert_eq!((p2.pn, p2.page_number, p2.is_first_page), (10, 2, false));
    assert_eq!(p2.next_pn, Some(20));
    assert_eq!(p2.previous_pn, Some(0));
    assert!(p2.next_url.as_deref().unwrap().contains("pn=20"));
    assert!(p2.previous_url.is_some());

    let p3 = parse_fixture("tianqi_pn20.html").page;
    assert_eq!((p3.pn, p3.page_number), (20, 3));
    assert_eq!(p3.next_url, None);
    assert_eq!(p3.next_pn, None);
    assert_eq!(p3.previous_pn, Some(10));
    assert!(p3.first_url.is_some());
}

#[test]
fn no_cross_page_duplicates() {
    let mut seen: Vec<String> = Vec::new();
    for name in ["tianqi_p1.html", "tianqi_pn10.html", "tianqi_pn20.html"] {
        for r in &parse_fixture(name).results {
            if let Some(u) = &r.url {
                assert!(!seen.contains(u), "duplicate url {} in {}", u, name);
                seen.push(u.clone());
            }
        }
    }
}

#[test]
fn related_searches() {
    let page = parse_fixture("tianqi_p1.html");
    let queries: Vec<&str> = page
        .related_searches
        .iter()
        .map(|r| r.query.as_str())
        .collect();
    assert_eq!(queries[0], "没有广告的天气预报官方版");
    assert_eq!(queries.len(), 10);
    assert!(page.related_searches.iter().all(|r| r.url.is_some()));
    let mut sources: Vec<&str> = page
        .related_searches
        .iter()
        .map(|r| r.source.as_deref().unwrap())
        .collect();
    sources.sort();
    sources.dedup();
    assert!(sources
        .iter()
        .all(|s| ["recommend_list", "page_relative"].contains(s)));
}

#[test]
fn json_serialisable_and_warnings_empty() {
    for name in [
        "tianqi_p1.html",
        "tianqi_pn10.html",
        "tianqi_pn20.html",
        "sapporo.html",
        "weather_en.html",
    ] {
        let data = parse_fixture(name).to_value();
        let text = serde_json::to_string(&data).unwrap();
        let back: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(back, data, "{} roundtrip", name);
        assert_eq!(data["parser"]["name"], "baidu_parser");
        assert_eq!(
            data["diagnostics"]["warnings"],
            serde_json::json!([]),
            "{} warnings",
            name
        );
    }
}
