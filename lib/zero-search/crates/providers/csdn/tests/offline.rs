// Offline parsing tests. The fixtures are trimmed REAL gateway
// responses captured live on 2026-11-07 (query "rust"), not
// synthetic data: ../live/raw-search.json and ../live/raw-detail.json
// hold the untouched captures.
use csdn_search::{
    app_subfilter, build_search_body, extract_article_id, normalize_sort, parse_article_response,
    parse_search_response, parse_search_response_with_options, validate_search_options,
    SearchCategory, SearchOptions,
};
use serde_json::Value;

fn fixture(name: &str) -> Value {
    let raw = match name {
        "search" => include_str!("fixtures/search.json"),
        "detail" => include_str!("fixtures/detail.json"),
        // Round-2 fixtures: trimmed REAL captures from 2026-11-08
        // (query "Java"), see ../live/round2/ for the untouched files.
        "blog" => include_str!("fixtures/search-blog.json"),
        "ask" => include_str!("fixtures/search-ask.json"),
        "course" => include_str!("fixtures/search-course.json"),
        "user" => include_str!("fixtures/search-user.json"),
        "vip" => include_str!("fixtures/search-vip.json"),
        _ => unreachable!(),
    };
    serde_json::from_str(raw).unwrap()
}

fn options<'a>(query: &'a str, category: SearchCategory) -> SearchOptions<'a> {
    let mut o = SearchOptions::new(query);
    o.category = category;
    o
}

#[test]
fn search_response_maps_items() {
    let out = parse_search_response(&fixture("search"), "rust", 1, 20);
    assert_eq!(out["provider"], "csdn");
    assert_eq!(out["ok"], true);
    assert_eq!(out["count"], 2);
    // The live v6 response carries no per-item total.
    assert!(out["total_available"].is_null());
    let first = &out["items"][0];
    assert_eq!(first["id"], "152301757");
    // Titles carry the server's <em> highlight markup verbatim.
    assert!(first["title"].as_str().unwrap().contains("<em>Rust</em>"));
    assert_eq!(first["author"], "gitblog_00700");
    assert_eq!(
        first["url"],
        "https://blog.csdn.net/gitblog_00700/article/details/152301757"
    );
    // viewcount arrives as a JSON string in the live response.
    assert_eq!(first["view_count"], 919);
    assert_eq!(first["published_at"], "2025-09-30 00:00:00");
    assert_eq!(out["items"][1]["id"], "87755263");
}

#[test]
fn article_response_maps_body() {
    let out = parse_article_response(&fixture("detail"), "152301757");
    assert_eq!(out["ok"], true);
    let item = &out["item"];
    assert_eq!(item["id"], "152301757");
    assert!(item["title"].as_str().unwrap().contains("Rust Enhanced"));
    assert_eq!(item["author"], "gitblog_00700");
    assert_eq!(item["author_name"], "成旭涛Strange");
    let html = item["content_html"].as_str().unwrap();
    assert!(html.len() > 10_000);
    let text = item["content_text"].as_str().unwrap();
    assert!(text.contains("Rust Enhanced"));
    assert!(!text.contains("<article"));
    assert_eq!(item["view_count"], 919);
    assert_eq!(item["comment_count"], 0);
    assert_eq!(item["digg_count"], 23);
    assert_eq!(item["collect_count"], 21);
    assert!(item["tags"].is_array());
}

#[test]
fn article_id_extraction() {
    assert_eq!(extract_article_id("152301757"), "152301757");
    assert_eq!(
        extract_article_id("https://blog.csdn.net/gitblog_00700/article/details/152301757"),
        "152301757"
    );
    assert_eq!(
        extract_article_id(
            "https://blog.csdn.net/gitblog_00700/article/details/152301757?spm=1001.2014.3001.5501"
        ),
        "152301757"
    );
}

#[test]
fn category_values_match_app() {
    // (public name, app type/block value), from the app request
    // classes (6.5.7 bi4/di4/ci4/ni4/ki4/jj4/pi4/uj4/ak4, 6.15.2
    // xg5 + SearchColumnFragment). The "all" value is the current
    // app's `all_v2`; `all` (6.5.7) parses to the same category.
    // There is no forum category: the app has no forum search.
    let cases = [
        ("all", "all_v2"),
        ("blog", "blog"),
        ("ask", "ask"),
        ("download", "download"),
        ("course", "appcourse"),
        ("mall", "sku"),
        ("ebook", "ebook"),
        ("user", "userinfo"),
        ("vip", "vip"),
        ("special_column", "special_column"),
    ];
    for (name, raw) in cases {
        let cat = SearchCategory::parse(name).unwrap();
        assert_eq!(cat.name(), name);
        assert_eq!(cat.request_value(), raw);
        // The raw app value parses back to the same category.
        assert_eq!(SearchCategory::parse(raw), Some(cat));
    }
    assert_eq!(SearchCategory::parse("all"), Some(SearchCategory::All));
    assert_eq!(SearchCategory::parse("forum"), None);
    assert_eq!(SearchCategory::parse("bbs"), None);
    assert_eq!(SearchCategory::parse("nonsense"), None);
}

#[test]
fn sort_normalization_matches_app_ui() {
    // SearchContainerFragment.Q: 综合 (no field), 最新, 热门.
    assert_eq!(normalize_sort(None), None);
    assert_eq!(normalize_sort(Some("relevance")), None);
    assert_eq!(
        normalize_sort(Some("latest")).as_deref(),
        Some("times_rank")
    );
    assert_eq!(
        normalize_sort(Some("times_rank")).as_deref(),
        Some("times_rank")
    );
    assert_eq!(normalize_sort(Some("popular")).as_deref(), Some("hot_rank"));
    assert_eq!(
        normalize_sort(Some("hot_rank")).as_deref(),
        Some("hot_rank")
    );
    // Unknown raw values pass through (round-1 behaviour).
    assert_eq!(normalize_sort(Some("other")).as_deref(), Some("other"));
}

#[test]
fn app_filter_expressions_match_container_fragment() {
    // SearchContainerFragment$a.onClick, verbatim expressions.
    assert_eq!(
        app_subfilter(SearchCategory::Blog, Some("vip_content")).as_deref(),
        Some("vip_view_auth=\"1\"")
    );
    assert_eq!(
        app_subfilter(SearchCategory::Mall, Some("vip_content")).as_deref(),
        Some("ext_1=1")
    );
    assert_eq!(
        app_subfilter(SearchCategory::Course, Some("vip_content")).as_deref(),
        Some("ext_21=\"2\"")
    );
    assert_eq!(
        app_subfilter(SearchCategory::Ask, Some("accepted")).as_deref(),
        Some("ext_21!=\"\"")
    );
    // Combinations the app does not offer.
    assert_eq!(app_subfilter(SearchCategory::Blog, Some("accepted")), None);
    assert_eq!(
        app_subfilter(SearchCategory::Download, Some("vip_content")),
        None
    );
}

#[test]
fn search_body_matches_app_classes() {
    // "all" (bi4/xg5) sets insert_card/completion_down/search_version/version;
    // the type/block value is the current app's `all_v2`.
    let body = build_search_body(&options("rust", SearchCategory::All));
    assert_eq!(body["channel"], "app_search_result_v6");
    let ext = &body["ext"];
    assert_eq!(ext["type"], "all_v2");
    assert_eq!(ext["block"], "all_v2");
    assert_eq!(ext["insert_card"], "1");
    assert_eq!(ext["completion_down"], 1);
    assert_eq!(ext["search_version"], "v4");
    assert_eq!(ext["version"], 1);
    assert!(ext.get("second_rank_name_sort").is_none());
    assert!(ext.get("subfilter").is_none());
    assert!(ext.get("search_user_name").is_none());

    // blog (di4) sets none of the four extras.
    let body = build_search_body(&options("rust", SearchCategory::Blog));
    let ext = &body["ext"];
    assert_eq!(ext["type"], "blog");
    for k in [
        "insert_card",
        "completion_down",
        "search_version",
        "version",
    ] {
        assert!(ext.get(k).is_none(), "blog body must omit {k}");
    }

    // download (ni4) sets completion_down only.
    let body = build_search_body(&options("rust", SearchCategory::Download));
    let ext = &body["ext"];
    assert_eq!(ext["completion_down"], 1);
    assert!(ext.get("insert_card").is_none());
    assert!(ext.get("search_version").is_none());

    // Sort + friendly filter land in the body; a raw subfilter wins
    // over the friendly filter.
    let mut o = options("rust", SearchCategory::Blog);
    o.sort = Some("latest");
    o.filter = Some("vip_content");
    let body = build_search_body(&o);
    assert_eq!(body["ext"]["second_rank_name_sort"], "times_rank");
    assert_eq!(body["ext"]["subfilter"], "vip_view_auth=\"1\"");
    o.subfilter = Some("digg>10");
    let body = build_search_body(&o);
    assert_eq!(body["ext"]["subfilter"], "digg>10");
}

#[test]
fn personal_home_body_switches_channel() {
    let mut o = options("rust", SearchCategory::Blog);
    o.search_user_name = Some("hollis_chuang");
    let body = build_search_body(&o);
    assert_eq!(body["channel"], "app_personal_home_search");
    assert_eq!(body["ext"]["search_user_name"], "hollis_chuang");
    // Personal-home search is unimplemented in the app for course.
    let mut o = options("rust", SearchCategory::Course);
    o.search_user_name = Some("hollis_chuang");
    assert!(validate_search_options(&o).is_err());
    // A filter the category does not offer is rejected too.
    let mut o = options("rust", SearchCategory::Blog);
    o.filter = Some("accepted");
    assert!(validate_search_options(&o).is_err());
}

#[test]
fn elastic_blog_fixture_maps_items() {
    let o = options("Java", SearchCategory::Blog);
    let out = parse_search_response_with_options(&fixture("blog"), &o);
    assert_eq!(out["ok"], true);
    assert_eq!(out["category"], "blog");
    assert_eq!(out["search_type"], "blog");
    assert_eq!(out["count"], 2);
    let first = &out["items"][0];
    assert_eq!(first["id"], "103272122");
    assert_eq!(first["author"], "hollis_chuang");
    assert_eq!(
        first["url"],
        "https://blog.csdn.net/hollis_chuang/article/details/103272122"
    );
    assert_eq!(first["view_count"], 20542);
    assert_eq!(first["published_at"], "2019-11-27 00:00:00");
    assert_eq!(first["type"], "blog");
}

#[test]
fn ask_fixture_maps_items() {
    let o = options("Java", SearchCategory::Ask);
    let out = parse_search_response_with_options(&fixture("ask"), &o);
    assert_eq!(out["count"], 2);
    let first = &out["items"][0];
    assert_eq!(first["id"], "7996817");
    assert_eq!(first["url"], "https://ask.csdn.net/questions/7996817");
    assert_eq!(first["view_count"], 7);
    assert_eq!(first["type"], "ask");
}

#[test]
fn course_fixture_maps_items() {
    let o = options("Java", SearchCategory::Course);
    let out = parse_search_response_with_options(&fixture("course"), &o);
    assert_eq!(out["count"], 2);
    let first = &out["items"][0];
    assert_eq!(first["id"], "772");
    assert!(first["title"].as_str().unwrap().contains("Java"));
    assert_eq!(first["url"], "https://edu.csdn.net/course/detail/772");
    assert_eq!(first["student_count"], 2681474);
}

#[test]
fn user_fixture_maps_items() {
    let o = options("Java", SearchCategory::User);
    let out = parse_search_response_with_options(&fixture("user"), &o);
    assert_eq!(out["count"], 2);
    let first = &out["items"][0];
    assert_eq!(first["id"], "2401_85773384");
    assert_eq!(first["title"], "Java Java Java");
    assert_eq!(first["fans_count"], 138);
    assert!(first["avatar"].as_str().unwrap().starts_with("https://"));
}

#[test]
fn vip_fixture_maps_items() {
    let o = options("Java", SearchCategory::Vip);
    let out = parse_search_response_with_options(&fixture("vip"), &o);
    assert_eq!(out["count"], 2);
    assert_eq!(out["items"][0]["id"], "12813303");
}

#[test]
fn html_to_text_handles_blocks_and_entities() {
    let text = csdn_search::html_to_text("<p>a</p><p>b&amp;c</p><pre>x&lt;y</pre>");
    assert!(text.contains("a\nb&c"));
    assert!(text.contains("x<y"));
}
