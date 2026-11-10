//! Offline tests for the unified library API surface: categories,
//! engine registries, the unified item type, and client
//! construction. No network access happens here; live behaviour is
//! covered by the server e2e tests and the harness example.

use zero_search::{
    ArticleOutcome, ArticleProvider, Category, CategoryOutcome, Client, EngineOutcome,
    MediaOutcome, MediaProvider, SearchItem, ShopOutcome, ShopProvider, WebEngine,
};

#[test]
fn categories_cover_nineteen_engines() {
    assert_eq!(Category::Web.id(), "web");
    assert_eq!(Category::Shopping.id(), "shopping");
    assert_eq!(Category::Media.id(), "media");
    assert_eq!(Category::Articles.id(), "articles");
    assert_eq!(
        Category::Web.engine_ids(),
        &["baidu", "yahoo_jp", "bing", "naver", "yahoo_us", "yandex"]
    );
    assert_eq!(
        Category::Shopping.engine_ids(),
        &[
            "aliexpress",
            "goofish",
            "mercari",
            "yahoo_auctions",
            "yahoo_fleamarket",
            "rakuma",
            "rakuten",
            "qoo10",
            "yahoo_shopping",
            "amazon"
        ]
    );
    assert_eq!(Category::Media.engine_ids(), &["youtube", "bilibili"]);
    assert_eq!(Category::Articles.engine_ids(), &["csdn"]);
    assert_eq!(Category::all().len(), 4);
    let total: usize = Category::all().iter().map(|c| c.engine_ids().len()).sum();
    assert_eq!(total, 19);
}

#[test]
fn engine_registries_are_complete_and_unique() {
    let web: Vec<&str> = WebEngine::all().iter().map(|e| e.id()).collect();
    assert_eq!(web.len(), 6);
    let mut sorted = web.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), 6, "web engine ids must be unique");
    assert_eq!(web, Category::Web.engine_ids());

    let shop: Vec<&str> = ShopProvider::all().iter().map(|p| p.id()).collect();
    assert_eq!(shop.len(), 10);
    let mut sorted = shop.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), 10, "shop provider ids must be unique");
    assert_eq!(shop, Category::Shopping.engine_ids());

    let media: Vec<&str> = MediaProvider::all().iter().map(|p| p.id()).collect();
    assert_eq!(media.len(), 2);
    let mut sorted = media.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), 2, "media provider ids must be unique");
    assert_eq!(media, Category::Media.engine_ids());

    let articles: Vec<&str> = ArticleProvider::all().iter().map(|p| p.id()).collect();
    assert_eq!(articles.len(), 1);
    assert_eq!(articles, Category::Articles.engine_ids());
}

#[test]
fn search_item_roundtrips_through_json() {
    let item = SearchItem {
        engine: WebEngine::Bing,
        title: "Example".to_string(),
        url: "https://example.com/".to_string(),
        snippet: Some("snippet".to_string()),
    };
    let text = serde_json::to_string(&item).unwrap();
    let back: SearchItem = serde_json::from_str(&text).unwrap();
    assert_eq!(item, back);
    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(value["engine"], serde_json::json!("bing"));
}

#[test]
fn outcome_types_serialise() {
    let web = CategoryOutcome::Web(vec![EngineOutcome {
        engine: WebEngine::Naver,
        ok: false,
        items: vec![],
        error: Some("boom".to_string()),
        error_kind: Some(zero_search::ErrorKind::Unknown),
    }]);
    let value0 = serde_json::to_value(&web).unwrap();
    assert_eq!(value0[0]["error_kind"], serde_json::json!("unknown"));
    let value = serde_json::to_value(&web).unwrap();
    assert_eq!(value[0]["engine"], serde_json::json!("naver"));
    assert_eq!(value[0]["ok"], serde_json::json!(false));

    let shop = CategoryOutcome::Shopping(vec![ShopOutcome {
        provider: ShopProvider::Mercari,
        ok: true,
        result: Some(serde_json::json!({"ok": true})),
        error: None,
        error_kind: None,
    }]);
    let value = serde_json::to_value(&shop).unwrap();
    assert_eq!(value[0]["provider"], serde_json::json!("mercari"));

    let media = CategoryOutcome::Media(vec![MediaOutcome {
        provider: MediaProvider::Bilibili,
        ok: true,
        result: Some(serde_json::json!({"ok": true})),
        error: None,
        error_kind: None,
    }]);
    let value = serde_json::to_value(&media).unwrap();
    assert_eq!(value[0]["provider"], serde_json::json!("bilibili"));

    let articles = CategoryOutcome::Articles(vec![ArticleOutcome {
        provider: ArticleProvider::Csdn,
        ok: true,
        result: Some(serde_json::json!({"ok": true})),
        error: None,
        error_kind: None,
    }]);
    let value = serde_json::to_value(&articles).unwrap();
    assert_eq!(value[0]["provider"], serde_json::json!("csdn"));
}

#[test]
fn client_constructs_without_configuration() {
    // Construction must not read config files, environment
    // variables or the network — it just builds the raw clients.
    let client = Client::new().expect("zero-config client construction");
    // The raw providers stay reachable through the facade.
    let _ = client.baidu();
    let _ = client.yahoo();
    let _ = client.bing();
    let _ = client.naver();
    let _ = client.yahoo_us();
    let _ = client.yandex();
    let _ = client.aliexpress();
    let _ = client.goofish();
    let _ = client.mercari();
    let _ = client.rakuma();
    let _ = client.yahoo_auctions();
    let _ = client.yahoo_flea();
    let _ = client.rakuten();
    let _ = client.qoo10();
    let _ = client.yahoo_shopping();
    let _ = client.amazon();
    let _ = client.youtube();
    let _ = client.bilibili();
    let _ = client.csdn();
    // And through the providers module under their original names.
    let _ = zero_search::providers::bing_search::PROVIDER;
    let _ = zero_search::providers::naver_search::PROVIDER;
    let _ = zero_search::providers::yahoo_search::PROVIDER;
}

#[test]
fn media_next_token_reads_each_providers_field() {
    use serde_json::json;
    // YouTube: `next_continuation`, null/absent at the end.
    let yt = json!({ "next_continuation": "tok123" });
    assert_eq!(
        zero_search::media_next_token(MediaProvider::YouTube, &yt).as_deref(),
        Some("tok123")
    );
    let yt_end = json!({ "next_continuation": null });
    assert_eq!(
        zero_search::media_next_token(MediaProvider::YouTube, &yt_end),
        None
    );
    // Bilibili: `next`, empty string at the end.
    let bili = json!({ "next": "n2" });
    assert_eq!(
        zero_search::media_next_token(MediaProvider::Bilibili, &bili).as_deref(),
        Some("n2")
    );
    let bili_end = json!({ "next": "" });
    assert_eq!(
        zero_search::media_next_token(MediaProvider::Bilibili, &bili_end),
        None
    );
    // Each provider reads only its own field.
    assert_eq!(
        zero_search::media_next_token(MediaProvider::YouTube, &bili),
        None
    );
    assert_eq!(
        zero_search::media_next_token(MediaProvider::Bilibili, &yt),
        None
    );
}

#[test]
fn error_kind_classifies_provider_messages() {
    use zero_search::ErrorKind;
    // The exact message shapes the providers produce (Z12).
    assert_eq!(
        ErrorKind::from_error_message(
            "access_restricted: HTTP 429 Too Many Requests from Bing (rate limited or blocked)"
        ),
        ErrorKind::RateLimited
    );
    assert_eq!(
        ErrorKind::from_error_message(
            "access_restricted: yandex answered with a smart captcha (showcaptcha)"
        ),
        ErrorKind::Captcha
    );
    assert_eq!(
        ErrorKind::from_error_message("access_restricted: HTTP 403 Forbidden"),
        ErrorKind::AccessRestricted
    );
    assert_eq!(
        ErrorKind::from_error_message("http_error: HTTP 500 from Yahoo"),
        ErrorKind::ServerError
    );
    assert_eq!(
        ErrorKind::from_error_message("yahoo us image search returned HTTP 500"),
        ErrorKind::ServerError
    );
    assert_eq!(
        ErrorKind::from_error_message("amazon returned HTTP 503"),
        ErrorKind::ServerError
    );
    assert_eq!(
        ErrorKind::from_error_message("http_error: HTTP 400 from Bing"),
        ErrorKind::HttpError
    );
    assert_eq!(
        ErrorKind::from_error_message("parse_error: no results found in the response"),
        ErrorKind::ParseError
    );
    assert_eq!(
        ErrorKind::from_error_message("connection_error: dns error"),
        ErrorKind::NetworkError
    );
    assert_eq!(
        ErrorKind::from_error_message("not_found: HTTP 404"),
        ErrorKind::NotFound
    );
    assert_eq!(
        ErrorKind::from_error_message("something entirely different"),
        ErrorKind::Unknown
    );
}

#[test]
fn error_kind_serialises_snake_case_and_is_omitted_on_success() {
    let ok = CategoryOutcome::Web(vec![EngineOutcome {
        engine: WebEngine::Bing,
        ok: true,
        items: vec![],
        error: None,
        error_kind: None,
    }]);
    let value = serde_json::to_value(&ok).unwrap();
    assert!(value[0].get("error_kind").is_none(), "{value}");
    assert_eq!(
        serde_json::to_value(zero_search::ErrorKind::RateLimited).unwrap(),
        serde_json::json!("rate_limited")
    );
    assert_eq!(
        serde_json::to_value(zero_search::ErrorKind::AccessRestricted).unwrap(),
        serde_json::json!("access_restricted")
    );
}
