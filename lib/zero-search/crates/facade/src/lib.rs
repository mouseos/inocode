//! `zero_search` — the ZeroSearch library.
//!
//! One zero-config entry point to nineteen search providers: six web
//! engines (Baidu, Yahoo! JAPAN, Bing, Naver, Yahoo US, Yandex), ten
//! shopping providers (AliExpress, Goofish, Mercari, Yahoo!
//! Auctions, Yahoo! Flea Market, Rakuma, Rakuten, Qoo10, Yahoo!
//! Shopping, Amazon), two media platforms (YouTube, Bilibili) and
//! one article platform (CSDN). No API key, no environment
//! variables, no configuration files — construct a [`Client`] (or
//! call the free functions) and search.
//!
//! The crate works on two levels:
//!
//! - The **unified API** in this module: [`Client::search`] /
//!   [`search`] for one web engine, [`Client::search_all`] /
//!   [`search_all`] for every web engine at once (per-engine
//!   success/failure kept separate), [`Client::search_category`] for
//!   a whole [`Category`], [`Client::search_products`] /
//!   [`search_products`] for shopping, [`Client::search_media`] /
//!   [`search_media`] for the media platforms, and
//!   [`Client::search_articles`] / [`search_articles`] for the
//!   article platforms. Web results are normalised into
//!   [`SearchItem`]. Media and article searches also have paged
//!   variants ([`Client::search_media_paged`],
//!   [`Client::search_articles_paged`]) that thread the providers'
//!   continuation tokens / page numbers; [`media_next_token`]
//!   extracts the token for the next page from a media result.
//! - The **raw providers**, re-exported under [`providers`] (and at
//!   the crate root under their original crate names), so every
//!   provider's own client, request types and result types stay
//!   reachable exactly as in the standalone crates.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Raw provider re-exports (original package / lib names preserved)
// ---------------------------------------------------------------------------

pub use baidu_search;
pub use bilibili_provider;
pub use bing_search;
pub use csdn_search;
pub use naver_search;
pub use yahoo_search;
pub use yahoo_us_search;
pub use yandex_search;
pub use youtube_search;

/// All nineteen provider crates under one module, by role.
pub mod providers {
    pub use baidu_search;
    pub use bing_search;
    pub use naver_search;
    pub use yahoo_search;
    pub use yahoo_us_search;
    pub use yandex_search;

    pub use shopping_mcp_aliexpress as aliexpress;
    pub use shopping_mcp_amazon as amazon;
    pub use shopping_mcp_goofish as goofish;
    pub use shopping_mcp_mercari as mercari;
    pub use shopping_mcp_qoo10 as qoo10;
    pub use shopping_mcp_rakuma as rakuma;
    pub use shopping_mcp_rakuten as rakuten;
    pub use shopping_mcp_yahoo_auctions as yahoo_auctions;
    pub use shopping_mcp_yahoo_flea as yahoo_flea;
    pub use shopping_mcp_yahoo_shopping as yahoo_shopping;

    pub use bilibili_provider as bilibili;
    pub use youtube_search as youtube;

    pub use csdn_search as csdn;
}

// ---------------------------------------------------------------------------
// Categories and engines
// ---------------------------------------------------------------------------

/// The four engine categories.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    /// General web search engines.
    Web,
    /// Shopping / marketplace search providers.
    Shopping,
    /// Media content platforms (videos).
    Media,
    /// Article platforms (technical articles and blogs).
    Articles,
}

impl Category {
    /// Stable category id (`"web"` / `"shopping"` / `"media"` /
    /// `"articles"`).
    pub fn id(&self) -> &'static str {
        match self {
            Category::Web => "web",
            Category::Shopping => "shopping",
            Category::Media => "media",
            Category::Articles => "articles",
        }
    }

    /// Ids of the engines that belong to this category.
    pub fn engine_ids(&self) -> &'static [&'static str] {
        match self {
            Category::Web => &["baidu", "yahoo_jp", "bing", "naver", "yahoo_us", "yandex"],
            Category::Shopping => &[
                "aliexpress",
                "goofish",
                "mercari",
                "yahoo_auctions",
                "yahoo_fleamarket",
                "rakuma",
                "rakuten",
                "qoo10",
                "yahoo_shopping",
                "amazon",
            ],
            Category::Media => &["youtube", "bilibili"],
            Category::Articles => &["csdn"],
        }
    }

    /// All four categories.
    pub fn all() -> &'static [Category] {
        &[
            Category::Web,
            Category::Shopping,
            Category::Media,
            Category::Articles,
        ]
    }
}

/// A media platform (category [`Category::Media`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaProvider {
    YouTube,
    Bilibili,
}

impl MediaProvider {
    /// Stable provider id.
    pub fn id(&self) -> &'static str {
        match self {
            MediaProvider::YouTube => "youtube",
            MediaProvider::Bilibili => "bilibili",
        }
    }

    /// Every media provider, in canonical order.
    pub fn all() -> &'static [MediaProvider] {
        &[MediaProvider::YouTube, MediaProvider::Bilibili]
    }
}

/// An article platform (category [`Category::Articles`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArticleProvider {
    Csdn,
}

impl ArticleProvider {
    /// Stable provider id.
    pub fn id(&self) -> &'static str {
        match self {
            ArticleProvider::Csdn => "csdn",
        }
    }

    /// Every article provider, in canonical order.
    pub fn all() -> &'static [ArticleProvider] {
        &[ArticleProvider::Csdn]
    }
}

/// A web search engine (category [`Category::Web`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebEngine {
    Baidu,
    YahooJapan,
    Bing,
    Naver,
    YahooUs,
    Yandex,
}

impl WebEngine {
    /// Stable engine id, as used in [`SearchItem::engine`] and the
    /// MCP `zero_list_categories` tool.
    pub fn id(&self) -> &'static str {
        match self {
            WebEngine::Baidu => "baidu",
            WebEngine::YahooJapan => "yahoo_jp",
            WebEngine::Bing => "bing",
            WebEngine::Naver => "naver",
            WebEngine::YahooUs => "yahoo_us",
            WebEngine::Yandex => "yandex",
        }
    }

    /// Every web engine, in canonical order.
    pub fn all() -> &'static [WebEngine] {
        &[
            WebEngine::Baidu,
            WebEngine::YahooJapan,
            WebEngine::Bing,
            WebEngine::Naver,
            WebEngine::YahooUs,
            WebEngine::Yandex,
        ]
    }
}

/// A shopping search provider (category [`Category::Shopping`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShopProvider {
    AliExpress,
    Goofish,
    Mercari,
    YahooAuctions,
    YahooFleaMarket,
    Rakuma,
    Rakuten,
    Qoo10,
    YahooShopping,
    Amazon,
}

impl ShopProvider {
    /// Stable provider id.
    pub fn id(&self) -> &'static str {
        match self {
            ShopProvider::AliExpress => "aliexpress",
            ShopProvider::Goofish => "goofish",
            ShopProvider::Mercari => "mercari",
            ShopProvider::YahooAuctions => "yahoo_auctions",
            ShopProvider::YahooFleaMarket => "yahoo_fleamarket",
            ShopProvider::Rakuma => "rakuma",
            ShopProvider::Rakuten => "rakuten",
            ShopProvider::Qoo10 => "qoo10",
            ShopProvider::YahooShopping => "yahoo_shopping",
            ShopProvider::Amazon => "amazon",
        }
    }

    /// Every shopping provider, in canonical order.
    pub fn all() -> &'static [ShopProvider] {
        &[
            ShopProvider::AliExpress,
            ShopProvider::Goofish,
            ShopProvider::Mercari,
            ShopProvider::YahooAuctions,
            ShopProvider::YahooFleaMarket,
            ShopProvider::Rakuma,
            ShopProvider::Rakuten,
            ShopProvider::Qoo10,
            ShopProvider::YahooShopping,
            ShopProvider::Amazon,
        ]
    }
}

// ---------------------------------------------------------------------------
// Unified result types
// ---------------------------------------------------------------------------

/// One web search result, normalised across engines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchItem {
    /// The engine that produced this result.
    pub engine: WebEngine,
    pub title: String,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
}

/// Machine-readable classification of a failed outcome.
///
/// Added so callers can react to the *kind* of failure — back off on
/// [`ErrorKind::RateLimited`], give up on [`ErrorKind::Captcha`],
/// retry on [`ErrorKind::NetworkError`] — without parsing the
/// human-readable `error` string, which is unchanged. The variants
/// mirror the error kinds the providers already use internally
/// (`access_restricted`, `http_error`, `parse_error`, …), classified
/// from the message by [`ErrorKind::from_error_message`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// The service throttled the request (HTTP 429 or an equivalent
    /// rate-limit response). Waiting and retrying later is sane.
    RateLimited,
    /// The service answered with a captcha / bot-verification
    /// challenge instead of results.
    Captcha,
    /// The service refused the request class outright (access
    /// restriction, risk-control or verification gates).
    AccessRestricted,
    /// The service itself failed (HTTP 5xx).
    ServerError,
    /// Any other HTTP error status (4xx and friends).
    HttpError,
    /// The response could not be parsed or carried no usable data.
    ParseError,
    /// The request never completed (connection failure, timeout).
    NetworkError,
    /// The requested resource does not exist (HTTP 404).
    NotFound,
    /// Anything that does not fit the kinds above.
    Unknown,
}

impl ErrorKind {
    /// Classify a provider error message. The providers prefix
    /// their errors with a kind tag (`access_restricted: …`,
    /// `http_error: …`, `parse_error: …`, `connection_error: …`,
    /// `not_found: …`); untagged messages are classified from the
    /// HTTP status or keywords they carry. This function is public
    /// so callers can classify errors from the single-provider
    /// methods (which return `anyhow::Error`) the same way the
    /// outcome types do.
    pub fn from_error_message(message: &str) -> ErrorKind {
        let lower = message.to_lowercase();
        if lower.contains("captcha") {
            return ErrorKind::Captcha;
        }
        if lower.contains("429") || lower.contains("too many requests") {
            return ErrorKind::RateLimited;
        }
        if lower.contains("not_found") || lower.contains("http 404") {
            return ErrorKind::NotFound;
        }
        if lower.contains("access_restricted") {
            return ErrorKind::AccessRestricted;
        }
        if lower.contains("parse_error") {
            return ErrorKind::ParseError;
        }
        if lower.contains("connection_error")
            || lower.contains("timeout")
            || lower.contains("timed out")
        {
            return ErrorKind::NetworkError;
        }
        // HTTP status mentions, tagged (`http_error: HTTP 503 …`)
        // or bare (`… returned HTTP 500`, reqwest's
        // `(500 Internal Server Error)`).
        for code in ["500", "502", "503", "504"] {
            if lower.contains(&format!("http {code}")) || lower.contains(&format!("({code} ")) {
                return ErrorKind::ServerError;
            }
        }
        if lower.contains("http_error") || lower.contains("http ") {
            return ErrorKind::HttpError;
        }
        ErrorKind::Unknown
    }
}

/// Extract the human-readable error text from a provider result
/// payload whose `ok` is false: the `error` field as a string, or
/// an error object's `message` (falling back to its `kind`).
fn payload_error_message(result: &Value) -> Option<String> {
    match result.get("error")? {
        Value::String(s) => Some(s.clone()),
        Value::Object(map) => map
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                map.get("kind")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            }),
        _ => None,
    }
}

/// The outcome of one engine inside a multi-engine search. Failures
/// are kept per engine: one engine failing never fails the others.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineOutcome {
    pub engine: WebEngine,
    pub ok: bool,
    #[serde(default)]
    pub items: Vec<SearchItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The failure's machine-readable kind; `None` on success.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<ErrorKind>,
}

/// The outcome of one provider inside a multi-provider shopping
/// search. `result` is the provider's own JSON payload (the exact
/// value the corresponding MCP tool returns).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShopOutcome {
    pub provider: ShopProvider,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The failure's machine-readable kind; `None` on success.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<ErrorKind>,
}

/// The outcome of one provider inside a multi-provider media
/// search. `result` is the provider's own JSON payload (the exact
/// value the corresponding MCP tool returns).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaOutcome {
    pub provider: MediaProvider,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The failure's machine-readable kind; `None` on success.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<ErrorKind>,
}

/// The outcome of one provider inside a multi-provider article
/// search. `result` is the provider's own JSON payload (the exact
/// value the corresponding MCP tool returns).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArticleOutcome {
    pub provider: ArticleProvider,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The failure's machine-readable kind; `None` on success.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<ErrorKind>,
}

/// The outcome of a category-wide search ([`Client::search_category`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CategoryOutcome {
    Web(Vec<EngineOutcome>),
    Shopping(Vec<ShopOutcome>),
    Media(Vec<MediaOutcome>),
    Articles(Vec<ArticleOutcome>),
}

// ---------------------------------------------------------------------------
// The unified client
// ---------------------------------------------------------------------------

/// A zero-config client holding one raw client per provider.
///
/// Construction performs no network access. All methods are async
/// and safe to call concurrently; multi-engine searches run their
/// engines sequentially (several providers rate-limit bursts).
pub struct Client {
    baidu: baidu_search::BaiduClient<baidu_search::ReqwestFetcher>,
    yahoo: yahoo_search::YahooSearchClient,
    bing: bing_search::BingSearchClient,
    naver: naver_search::NaverSearchClient,
    yahoo_us: yahoo_us_search::YahooUsSearchClient,
    yandex: yandex_search::YandexSearchClient,
    aliexpress: shopping_mcp_aliexpress::AliExpressClient,
    goofish: shopping_mcp_goofish::GoofishClient,
    mercari: shopping_mcp_mercari::MercariClient,
    rakuma: shopping_mcp_rakuma::RakumaClient,
    yauction: shopping_mcp_yahoo_auctions::YahooAuctionsClient,
    yflea: shopping_mcp_yahoo_flea::YahooFleaClient,
    rakuten: shopping_mcp_rakuten::RakutenClient,
    qoo10: shopping_mcp_qoo10::Qoo10Client,
    yshopping: shopping_mcp_yahoo_shopping::YahooShoppingClient,
    amazon: shopping_mcp_amazon::AmazonClient,
    youtube: youtube_search::YoutubeClient,
    bilibili: bilibili_provider::BilibiliClient,
    csdn: csdn_search::CsdnClient,
}

impl Client {
    /// Build a client. No configuration is read from anywhere; the
    /// only fallible step is the Bing client's internal setup.
    pub fn new() -> Result<Self> {
        Ok(Self {
            baidu: baidu_search::BaiduClient::new_live(),
            yahoo: yahoo_search::YahooSearchClient::new(),
            bing: bing_search::BingSearchClient::new()?,
            naver: naver_search::NaverSearchClient::new(),
            yahoo_us: yahoo_us_search::YahooUsSearchClient::new(),
            yandex: yandex_search::YandexSearchClient::new(),
            aliexpress: shopping_mcp_aliexpress::AliExpressClient::new(),
            goofish: shopping_mcp_goofish::GoofishClient::new(),
            mercari: shopping_mcp_mercari::MercariClient::new(),
            rakuma: shopping_mcp_rakuma::RakumaClient::new(),
            yauction: shopping_mcp_yahoo_auctions::YahooAuctionsClient::new(),
            yflea: shopping_mcp_yahoo_flea::YahooFleaClient::new(),
            rakuten: shopping_mcp_rakuten::RakutenClient::new(),
            qoo10: shopping_mcp_qoo10::Qoo10Client::new(),
            yshopping: shopping_mcp_yahoo_shopping::YahooShoppingClient::new(),
            amazon: shopping_mcp_amazon::AmazonClient::new(),
            youtube: youtube_search::YoutubeClient::new(),
            bilibili: bilibili_provider::BilibiliClient::new(),
            csdn: csdn_search::CsdnClient::new(),
        })
    }

    /// Access the raw provider clients (escape hatch to each
    /// provider's full API surface).
    pub fn baidu(&self) -> &baidu_search::BaiduClient<baidu_search::ReqwestFetcher> {
        &self.baidu
    }
    pub fn yahoo(&self) -> &yahoo_search::YahooSearchClient {
        &self.yahoo
    }
    pub fn bing(&self) -> &bing_search::BingSearchClient {
        &self.bing
    }
    pub fn naver(&self) -> &naver_search::NaverSearchClient {
        &self.naver
    }
    pub fn yahoo_us(&self) -> &yahoo_us_search::YahooUsSearchClient {
        &self.yahoo_us
    }
    pub fn yandex(&self) -> &yandex_search::YandexSearchClient {
        &self.yandex
    }
    pub fn aliexpress(&self) -> &shopping_mcp_aliexpress::AliExpressClient {
        &self.aliexpress
    }
    pub fn goofish(&self) -> &shopping_mcp_goofish::GoofishClient {
        &self.goofish
    }
    pub fn mercari(&self) -> &shopping_mcp_mercari::MercariClient {
        &self.mercari
    }
    pub fn rakuma(&self) -> &shopping_mcp_rakuma::RakumaClient {
        &self.rakuma
    }
    pub fn yahoo_auctions(&self) -> &shopping_mcp_yahoo_auctions::YahooAuctionsClient {
        &self.yauction
    }
    pub fn yahoo_flea(&self) -> &shopping_mcp_yahoo_flea::YahooFleaClient {
        &self.yflea
    }
    pub fn rakuten(&self) -> &shopping_mcp_rakuten::RakutenClient {
        &self.rakuten
    }
    pub fn qoo10(&self) -> &shopping_mcp_qoo10::Qoo10Client {
        &self.qoo10
    }
    pub fn yahoo_shopping(&self) -> &shopping_mcp_yahoo_shopping::YahooShoppingClient {
        &self.yshopping
    }
    pub fn amazon(&self) -> &shopping_mcp_amazon::AmazonClient {
        &self.amazon
    }
    pub fn youtube(&self) -> &youtube_search::YoutubeClient {
        &self.youtube
    }
    pub fn bilibili(&self) -> &bilibili_provider::BilibiliClient {
        &self.bilibili
    }
    pub fn csdn(&self) -> &csdn_search::CsdnClient {
        &self.csdn
    }

    /// Search one web engine (first page, provider-default size).
    pub async fn search(&self, engine: WebEngine, query: &str) -> Result<Vec<SearchItem>> {
        self.search_paged(engine, query, 1, 10).await
    }

    /// Search one web engine with paging.
    ///
    /// `page` is 1-based. Baidu's client aggregates from the first
    /// page and has no offset API, so for Baidu this returns the
    /// first aggregated page regardless of `page`/`limit`.
    pub async fn search_paged(
        &self,
        engine: WebEngine,
        query: &str,
        page: i64,
        limit: i64,
    ) -> Result<Vec<SearchItem>> {
        let page = page.max(1);
        let limit = limit.max(1);
        match engine {
            WebEngine::Baidu => {
                let agg = self.baidu.search(query, 1).await?;
                Ok(agg
                    .results
                    .iter()
                    .filter_map(|r| {
                        let title = r.get("title").and_then(Value::as_str)?.to_string();
                        let url = r.get("url").and_then(Value::as_str)?.to_string();
                        Some(SearchItem {
                            engine,
                            title,
                            url,
                            snippet: r.get("snippet").and_then(Value::as_str).map(str::to_string),
                        })
                    })
                    .collect())
            }
            WebEngine::YahooJapan => {
                let res = self.yahoo.web_search(query, page, limit, "", "").await?;
                Ok(res
                    .items
                    .into_iter()
                    .map(|it| SearchItem {
                        engine,
                        title: it.title,
                        url: it.url,
                        snippet: it.description,
                    })
                    .collect())
            }
            WebEngine::Bing => {
                let res = self.bing.web_search(query, page, limit, "", "").await?;
                Ok(res
                    .items
                    .into_iter()
                    .map(|it| SearchItem {
                        engine,
                        title: it.title,
                        url: it.url,
                        snippet: it.snippet,
                    })
                    .collect())
            }
            WebEngine::Naver => {
                let res = self.naver.web_search(query, page, limit).await?;
                Ok(res
                    .items
                    .into_iter()
                    .map(|it| SearchItem {
                        engine,
                        title: it.title,
                        url: it.url,
                        snippet: it.snippet,
                    })
                    .collect())
            }
            WebEngine::YahooUs => {
                let res = self.yahoo_us.web_search(query, page, limit).await?;
                Ok(res
                    .items
                    .into_iter()
                    .map(|it| SearchItem {
                        engine,
                        title: it.title,
                        url: it.url,
                        snippet: it.description,
                    })
                    .collect())
            }
            WebEngine::Yandex => {
                let res = self.yandex.web_search(query, page, limit).await?;
                Ok(res
                    .items
                    .into_iter()
                    .map(|it| SearchItem {
                        engine,
                        title: it.title,
                        url: it.url,
                        snippet: it.description,
                    })
                    .collect())
            }
        }
    }

    /// Search every web engine with the same query. Engines run
    /// sequentially; each engine's success or failure is reported
    /// separately in its [`EngineOutcome`].
    pub async fn search_all(&self, query: &str) -> Vec<EngineOutcome> {
        let mut out = Vec::with_capacity(WebEngine::all().len());
        for &engine in WebEngine::all() {
            out.push(self.search_outcome(engine, query).await);
        }
        out
    }

    async fn search_outcome(&self, engine: WebEngine, query: &str) -> EngineOutcome {
        match self.search(engine, query).await {
            Ok(items) => EngineOutcome {
                engine,
                ok: true,
                items,
                error: None,
                error_kind: None,
            },
            Err(e) => {
                let message = format!("{e:#}");
                EngineOutcome {
                    engine,
                    ok: false,
                    items: Vec::new(),
                    error_kind: Some(ErrorKind::from_error_message(&message)),
                    error: Some(message),
                }
            }
        }
    }

    /// Search one shopping provider. The returned JSON is the
    /// provider's own payload — what the matching MCP tool
    /// (`<provider>_search_products`) returns with default options,
    /// except that `limit` is passed through as given. Defaults
    /// mirror the MCP tools (e.g. AliExpress `ja_JP`/`JPY`/
    /// ship-to `JP`).
    pub async fn search_products(
        &self,
        provider: ShopProvider,
        query: &str,
        limit: i64,
    ) -> Result<Value> {
        let limit = limit.max(1);
        // The shopping providers take i32 limits (as do their MCP
        // tools); the facade API uses i64, so clamp on conversion.
        let limit_i32 = i32::try_from(limit).unwrap_or(i32::MAX);
        match provider {
            ShopProvider::AliExpress => Ok(self
                .aliexpress
                .search_products(query, 1, limit_i32, "ja_JP", "JPY", "JP", "")
                .await),
            ShopProvider::Goofish => Ok(self
                .goofish
                .search_products(query, 1, limit, None, None, None)
                .await),
            ShopProvider::Mercari => {
                let params = shopping_mcp_mercari::SearchParams {
                    query: query.to_string(),
                    limit: limit_i32,
                    page_token: String::new(),
                    sort: "default".to_string(),
                    status: String::new(),
                    exclude_keyword: String::new(),
                    category_ids: String::new(),
                    brand_ids: String::new(),
                    size_ids: String::new(),
                    seller_ids: String::new(),
                    price_min: None,
                    price_max: None,
                    item_condition_ids: String::new(),
                    shipping_payer_ids: String::new(),
                    color_ids: String::new(),
                    item_types: String::new(),
                    exclude_auction: false,
                    ..Default::default()
                };
                Ok(self.mercari.search_products(params).await?)
            }
            ShopProvider::YahooAuctions => {
                let params = shopping_mcp_yahoo_auctions::SearchParams {
                    query: query.to_string(),
                    limit: limit_i32,
                    page: 1,
                    sort: String::new(),
                    min_price: None,
                    max_price: None,
                    category_id: String::new(),
                    status: "open".to_string(),
                    keywords_and: String::new(),
                    keywords_not: String::new(),
                    seller_id: String::new(),
                    buynow_only: false,
                    free_shipping: false,
                    condition: String::new(),
                    ..Default::default()
                };
                let result = self.yauction.search(params).await?;
                Ok(serde_json::to_value(result)?)
            }
            ShopProvider::YahooFleaMarket => {
                let params = shopping_mcp_yahoo_flea::SearchParams {
                    query: query.to_string(),
                    limit: limit_i32,
                    page: 1,
                    sort: String::new(),
                    min_price: None,
                    max_price: None,
                    category_id: String::new(),
                };
                let result = self.yflea.search(params).await?;
                Ok(serde_json::to_value(result)?)
            }
            ShopProvider::Rakuma => {
                let params = shopping_mcp_rakuma::SearchParams {
                    query: query.to_string(),
                    limit: limit_i32,
                    page: 1,
                    sort: String::new(),
                    min_price: None,
                    max_price: None,
                    category_id: String::new(),
                    brand_id: String::new(),
                };
                let result = self.rakuma.search(params).await?;
                Ok(serde_json::to_value(result)?)
            }
            ShopProvider::Rakuten => {
                let params = shopping_mcp_rakuten::SearchParams {
                    query: query.to_string(),
                    limit: limit_i32,
                    ..Default::default()
                };
                let result = self.rakuten.search_products(params).await?;
                Ok(serde_json::to_value(result)?)
            }
            ShopProvider::Qoo10 => {
                let params = shopping_mcp_qoo10::SearchParams {
                    query: query.to_string(),
                    limit: limit_i32,
                    ..Default::default()
                };
                let result = self.qoo10.search_products(params).await?;
                Ok(serde_json::to_value(result)?)
            }
            ShopProvider::YahooShopping => {
                let params = shopping_mcp_yahoo_shopping::SearchParams {
                    query: query.to_string(),
                    limit: limit_i32,
                    ..Default::default()
                };
                let result = self.yshopping.search_products(params).await?;
                Ok(serde_json::to_value(result)?)
            }
            ShopProvider::Amazon => {
                let params = shopping_mcp_amazon::SearchParams {
                    query: query.to_string(),
                    limit: limit_i32,
                    ..Default::default()
                };
                let result = self.amazon.search_products(params).await?;
                Ok(serde_json::to_value(result)?)
            }
        }
    }

    /// Search every shopping provider with the same query. Each
    /// provider's success or failure is reported separately.
    /// Providers run sequentially and each performs a live request,
    /// so callers should treat this as ten searches, not one.
    pub async fn search_products_all(&self, query: &str, limit: i64) -> Vec<ShopOutcome> {
        let mut out = Vec::with_capacity(ShopProvider::all().len());
        for &provider in ShopProvider::all() {
            let outcome = match self.search_products(provider, query, limit).await {
                Ok(result) => {
                    let ok = result.get("ok").and_then(Value::as_bool).unwrap_or(true);
                    let error = if ok { None } else { payload_error_message(&result) };
                    let error_kind = error
                        .as_deref()
                        .map(ErrorKind::from_error_message);
                    ShopOutcome {
                        provider,
                        ok,
                        result: Some(result),
                        error,
                        error_kind,
                    }
                }
                Err(e) => {
                    let message = format!("{e:#}");
                    ShopOutcome {
                        provider,
                        ok: false,
                        result: None,
                        error_kind: Some(ErrorKind::from_error_message(&message)),
                        error: Some(message),
                    }
                }
            };
            out.push(outcome);
        }
        out
    }

    /// Search one media provider. The returned JSON is the
    /// provider's own payload — what the matching MCP tool
    /// (`youtube_search_videos` or `bilibili_search_videos`) returns
    /// with default options, except that `limit` bounds the page:
    /// Bilibili passes it to the service, YouTube returns its app
    /// page trimmed to `limit` items (its continuation token stays
    /// valid).
    pub async fn search_media(
        &self,
        provider: MediaProvider,
        query: &str,
        limit: i64,
    ) -> Result<Value> {
        let limit = limit.max(1);
        match provider {
            MediaProvider::YouTube => {
                let filters = youtube_search::SearchFilters::default();
                match self.youtube.search_videos(query, &filters).await {
                    Ok(page) => Ok(youtube_page_value(page.to_value(query), limit)),
                    Err(e) => Ok(serde_json::json!({
                        "provider": "youtube",
                        "ok": false,
                        "error": e.to_value(),
                    })),
                }
            }
            MediaProvider::Bilibili => {
                let page_size = i32::try_from(limit).unwrap_or(20);
                match self.bilibili.search_videos(query, page_size, "", 0).await {
                    Ok(value) => Ok(value),
                    Err(e) => Ok(bilibili_provider::error_json(&e)),
                }
            }
        }
    }

    /// Search one media provider, following a pagination token.
    ///
    /// `page_token` is the opaque continuation for the next page —
    /// the value [`media_next_token`] extracts from a previous
    /// result (`next_continuation` for YouTube, `next` for
    /// Bilibili). Pass an empty token for the first page; the call
    /// then behaves exactly like [`Client::search_media`]. The
    /// returned JSON is the provider's own payload, including the
    /// token for the page after it. YouTube continuation pages are
    /// trimmed to `limit` items just like first pages; Bilibili
    /// receives `limit` as its page size.
    pub async fn search_media_paged(
        &self,
        provider: MediaProvider,
        query: &str,
        limit: i64,
        page_token: &str,
    ) -> Result<Value> {
        let limit = limit.max(1);
        match provider {
            MediaProvider::YouTube => {
                if page_token.is_empty() {
                    return self.search_media(provider, query, limit).await;
                }
                match self.youtube.search_continuation(page_token).await {
                    Ok(page) => Ok(youtube_page_value(page.to_value(query), limit)),
                    Err(e) => Ok(serde_json::json!({
                        "provider": "youtube",
                        "ok": false,
                        "error": e.to_value(),
                    })),
                }
            }
            MediaProvider::Bilibili => {
                let page_size = i32::try_from(limit).unwrap_or(20);
                match self
                    .bilibili
                    .search_videos(query, page_size, page_token, 0)
                    .await
                {
                    Ok(value) => Ok(value),
                    Err(e) => Ok(bilibili_provider::error_json(&e)),
                }
            }
        }
    }

    /// Search every media provider with the same query. Each
    /// provider's success or failure is reported separately.
    /// Providers run sequentially and each performs a live request,
    /// so callers should treat this as two searches, not one.
    pub async fn search_media_all(&self, query: &str, limit: i64) -> Vec<MediaOutcome> {
        let mut out = Vec::with_capacity(MediaProvider::all().len());
        for &provider in MediaProvider::all() {
            let outcome = match self.search_media(provider, query, limit).await {
                Ok(result) => {
                    let ok = result.get("ok").and_then(Value::as_bool).unwrap_or(true);
                    let error = if ok { None } else { payload_error_message(&result) };
                    let error_kind = error
                        .as_deref()
                        .map(ErrorKind::from_error_message);
                    MediaOutcome {
                        provider,
                        ok,
                        result: Some(result),
                        error,
                        error_kind,
                    }
                }
                Err(e) => {
                    let message = format!("{e:#}");
                    MediaOutcome {
                        provider,
                        ok: false,
                        result: None,
                        error_kind: Some(ErrorKind::from_error_message(&message)),
                        error: Some(message),
                    }
                }
            };
            out.push(outcome);
        }
        out
    }

    /// Search one article provider. The returned JSON is the
    /// provider's own payload — what the matching MCP tool
    /// (`csdn_search_articles`) returns with default options, except
    /// that `limit` bounds the page and is passed to the service.
    pub async fn search_articles(
        &self,
        provider: ArticleProvider,
        query: &str,
        limit: i64,
    ) -> Result<Value> {
        self.search_articles_paged(provider, query, 1, limit).await
    }

    /// Search one article provider with an explicit page. Article
    /// providers page by number rather than by token (CSDN —
    /// currently the only one — takes a 1-based `page`), so this
    /// is the articles counterpart of
    /// [`Client::search_media_paged`]. `page` is 1-based; values
    /// below 1 are clamped to the first page.
    pub async fn search_articles_paged(
        &self,
        provider: ArticleProvider,
        query: &str,
        page: u32,
        limit: i64,
    ) -> Result<Value> {
        let limit = limit.max(1);
        let page = page.max(1);
        match provider {
            ArticleProvider::Csdn => Ok(csdn_search::search_articles(
                &self.csdn,
                query,
                page,
                u32::try_from(limit).unwrap_or(20).clamp(1, 50),
                None,
            )
            .await),
        }
    }

    /// Search every article provider with the same query. Each
    /// provider's success or failure is reported separately.
    /// Providers run sequentially and each performs a live request.
    pub async fn search_articles_all(&self, query: &str, limit: i64) -> Vec<ArticleOutcome> {
        let mut out = Vec::with_capacity(ArticleProvider::all().len());
        for &provider in ArticleProvider::all() {
            let outcome = match self.search_articles(provider, query, limit).await {
                Ok(result) => {
                    let ok = result.get("ok").and_then(Value::as_bool).unwrap_or(true);
                    let error = if ok { None } else { payload_error_message(&result) };
                    let error_kind = error
                        .as_deref()
                        .map(ErrorKind::from_error_message);
                    ArticleOutcome {
                        provider,
                        ok,
                        result: Some(result),
                        error,
                        error_kind,
                    }
                }
                Err(e) => {
                    let message = format!("{e:#}");
                    ArticleOutcome {
                        provider,
                        ok: false,
                        result: None,
                        error_kind: Some(ErrorKind::from_error_message(&message)),
                        error: Some(message),
                    }
                }
            };
            out.push(outcome);
        }
        out
    }

    /// Search a whole category with one query: every web engine for
    /// [`Category::Web`], every shopping provider for
    /// [`Category::Shopping`] (with the shopping default limit of
    /// 10 per provider), every media provider for
    /// [`Category::Media`] and every article provider for
    /// [`Category::Articles`] (both with a limit of 10 per provider).
    pub async fn search_category(&self, category: Category, query: &str) -> CategoryOutcome {
        match category {
            Category::Web => CategoryOutcome::Web(self.search_all(query).await),
            Category::Shopping => {
                CategoryOutcome::Shopping(self.search_products_all(query, 10).await)
            }
            Category::Media => CategoryOutcome::Media(self.search_media_all(query, 10).await),
            Category::Articles => {
                CategoryOutcome::Articles(self.search_articles_all(query, 10).await)
            }
        }
    }
}

/// Trim a YouTube search page's payload to `limit` items, keeping
/// its continuation token valid (shared by the first-page and
/// continuation paths).
fn youtube_page_value(mut value: Value, limit: i64) -> Value {
    if let Some(items) = value["items"].as_array_mut() {
        items.truncate(limit as usize);
        let count = items.len();
        value["count"] = serde_json::json!(count);
    }
    value
}

/// Extract the pagination token for the next page from a media
/// search result produced by [`Client::search_media`] /
/// [`Client::search_media_paged`]. Returns `None` when the result
/// carries no token (or an empty one) — i.e. there is no next
/// page. YouTube names the field `next_continuation`, Bilibili
/// `next`; feed the returned token back as `page_token`.
pub fn media_next_token(provider: MediaProvider, result: &Value) -> Option<String> {
    let field = match provider {
        MediaProvider::YouTube => "next_continuation",
        MediaProvider::Bilibili => "next",
    };
    result
        .get(field)
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
}

// ---------------------------------------------------------------------------
// Free functions (build a Client per call — the zero-config shortcut)
// ---------------------------------------------------------------------------

/// Search one web engine with a throwaway client. Harnesses that
/// search repeatedly should keep a [`Client`] instead.
pub async fn search(engine: WebEngine, query: &str) -> Result<Vec<SearchItem>> {
    Client::new()?.search(engine, query).await
}

/// Search every web engine with a throwaway client.
pub async fn search_all(query: &str) -> Result<Vec<EngineOutcome>> {
    Ok(Client::new()?.search_all(query).await)
}

/// Search one shopping provider with a throwaway client.
pub async fn search_products(provider: ShopProvider, query: &str, limit: i64) -> Result<Value> {
    Client::new()?.search_products(provider, query, limit).await
}

/// Search one media provider with a throwaway client.
pub async fn search_media(provider: MediaProvider, query: &str, limit: i64) -> Result<Value> {
    Client::new()?.search_media(provider, query, limit).await
}

/// Search one article provider with a throwaway client.
pub async fn search_articles(provider: ArticleProvider, query: &str, limit: i64) -> Result<Value> {
    Client::new()?.search_articles(provider, query, limit).await
}

/// Search one media provider, following a pagination token, with
/// a throwaway client.
pub async fn search_media_paged(
    provider: MediaProvider,
    query: &str,
    limit: i64,
    page_token: &str,
) -> Result<Value> {
    Client::new()?
        .search_media_paged(provider, query, limit, page_token)
        .await
}

/// Search one article provider with an explicit page, with a
/// throwaway client.
pub async fn search_articles_paged(
    provider: ArticleProvider,
    query: &str,
    page: u32,
    limit: i64,
) -> Result<Value> {
    Client::new()?
        .search_articles_paged(provider, query, page, limit)
        .await
}
