# ZeroSearch MCP

Six search engines, ten shopping sites, two media platforms and one article platform behind one MCP server.

**The only setup is registering the server. No API key. No environment variables.**
Point your MCP config at the binary and it works.

Web search: Baidu, Yahoo! JAPAN, Bing, Naver, Yahoo (US), Yandex.
Shopping search: AliExpress, Goofish (Xianyu), Mercari, Yahoo! Auctions, Yahoo! Flea Market, Rakuma, Rakuten Ichiba, Qoo10, Yahoo! Shopping, Amazon.co.jp.
Media: YouTube, Bilibili. Articles: CSDN.
Every provider works anonymously, with no login.

It also ships as a Rust library, with a unified API meant to be dropped into your own agent harness as its default search tool.

## Categories and engines

Engines fall into four categories. The `zero_list_categories` tool returns this mapping over MCP.

| Category | Engines |
|---|---|
| `web` | `baidu`, `yahoo_jp`, `bing`, `naver`, `yahoo_us`, `yandex` |
| `shopping` | `aliexpress`, `goofish`, `mercari`, `yahoo_auctions`, `yahoo_fleamarket`, `rakuma`, `rakuten`, `qoo10`, `yahoo_shopping`, `amazon` |
| `media` | `youtube`, `bilibili` |
| `articles` | `csdn` |

## Tools (129 total)

Tool names, arguments and result JSON are identical to the standalone servers this project merged, with one exception: `goofish_get_comments` replaced its `page` argument with `rows_per_page` in v0.4.0 (the app's API has no page number for comments; the argument is the number of comments per call, default 10).

### web

| Engine | Tools |
|---|---|
| Baidu (6) | `baidu_search`, `baidu_search_page`, `baidu_suggest`, `baidu_parse_search_results`, `baidu_build_search_url`, `baidu_list_sources` |
| Yahoo! JAPAN (8) | `yahoo_web_search`, `yahoo_image_search`, `yahoo_video_search`, `yahoo_news_search`, `yahoo_realtime_search`, `yahoo_chiebukuro_search`, `yahoo_suggest_keywords`, `yahoo_list_verticals` |
| Bing (10) | `bing_web_search`, `bing_image_search`, `bing_video_search`, `bing_news_search`, `bing_suggest`, `bing_shopping_search`, `bing_trending_topics`, `bing_visual_search`, `bing_build_search_url`, `bing_list_verticals` |
| Naver (11) | `naver_search`, `naver_web_search`, `naver_image_search`, `naver_news_search`, `naver_video_search`, `naver_blog_search`, `naver_cafe_search`, `naver_kin_search`, `naver_suggest`, `naver_build_search_url`, `naver_list_verticals` |
| Yahoo (US) (7) | `yahoo_us_web_search`, `yahoo_us_video_search`, `yahoo_us_image_search`, `yahoo_us_news_search`, `yahoo_us_suggest`, `yahoo_us_build_search_url`, `yahoo_us_list_verticals` |
| Yandex (7) | `yandex_web_search`, `yandex_image_search`, `yandex_video_search`, `yandex_suggest`, `yandex_suggest_endings`, `yandex_build_search_url`, `yandex_list_verticals` |

### shopping

| Provider | Tools |
|---|---|
| AliExpress (10) | `aliexpress_search_products`, `aliexpress_get_product`, `aliexpress_suggest_keywords`, `aliexpress_get_reviews`, `aliexpress_query_sku`, `aliexpress_adjust_sku`, `aliexpress_adjust_pdp`, `aliexpress_get_store_groups`, `aliexpress_get_store_module`, `aliexpress_search_store_products` |
| Goofish (19) | `goofish_search_products`, `goofish_get_product`, `goofish_suggest_keywords`, `goofish_get_comments`, `goofish_get_recommendations`, `goofish_home_feed`, `goofish_get_similar_items`, `goofish_get_category_guide`, `goofish_get_rate_list`, `goofish_get_user_headinfo`, `goofish_get_seller_items`, `goofish_get_city_feed`, `goofish_get_city_nearby`, `goofish_get_fishpond_list`, `goofish_get_fishpond_info`, `goofish_get_fishpond_comments`, `goofish_get_video_play`, `goofish_get_video_play_from_feed_card`, `goofish_get_fun_comments` |
| Mercari (7) | `mercari_search_products`, `mercari_get_product`, `mercari_search_seller_products`, `mercari_suggest_keywords`, `mercari_search_brands`, `mercari_search_categories`, `mercari_get_reviews` |
| Yahoo! Auctions (8) | `yauction_search_products`, `yauction_get_product`, `yauction_get_statistics`, `yauction_get_statistics_closed`, `yauction_get_seller_profile`, `yauction_get_store_profile`, `yauction_get_reviews`, `yauction_suggest_keywords` |
| Yahoo! Flea Market (3) | `yflea_search_products`, `yflea_get_product`, `yflea_get_reviews` |
| Rakuma (3) | `rakuma_search_products`, `rakuma_get_product`, `rakuma_get_reviews` |
| Rakuten Ichiba (7) | `rakuten_search_products`, `rakuten_get_product`, `rakuten_get_reviews`, `rakuten_suggest_keywords`, `rakuten_search_products_aggregated`, `rakuten_get_genre_tree`, `rakuten_get_variant_details` |
| Qoo10 (4) | `qoo10_search_products`, `qoo10_get_product`, `qoo10_get_search_filters`, `qoo10_suggest_keywords` |
| Yahoo! Shopping (4) | `yshopping_search_products`, `yshopping_get_product`, `yshopping_get_reviews`, `yshopping_suggest_keywords` |
| Amazon.co.jp (3) | `amazon_search_products`, `amazon_get_product`, `amazon_suggest_keywords` |
| Shared (1) | `shopping_list_sources` |

### media

| Provider | Tools |
|---|---|
| YouTube (5) | `youtube_search_videos`, `youtube_get_video`, `youtube_get_related`, `youtube_get_captions`, `youtube_get_comments` |
| Bilibili (3) | `bilibili_search_videos`, `bilibili_get_video`, `bilibili_get_related` |

### articles

| Provider | Tools |
|---|---|
| CSDN (2) | `csdn_search_articles`, `csdn_get_article` |

### ZeroSearch

| Tool | What it does |
|---|---|
| `zero_list_categories` | Lists the categories, their engines, and each engine's tools |

## Notes on the providers added in v0.2.0

- **Rakuten Ichiba**: uses the app's guest BFF as-is; no authentication. The detail endpoint can answer HTTP 207 for partial success — that is its normal form, and success is judged from the core item data.
- **Qoo10**: uses the app's mobile API; sorting and filtering cover every search option the app offers. Product detail has no native API, so the app's WebView page is reproduced instead. The device-derived token in the user agent cannot be reproduced off-device, so a format-preserving placeholder is sent in its place; live verification confirmed the service accepts it. The service does not respond to non-Japan-side access.
- **Yahoo! Shopping**: search and detail use the app's native API; the appid is a constant built into the app and no authentication is needed. The native API has no review-list endpoint, so reviews are read from the review page the app itself opens in a WebView (shopping.yahoo.co.jp/review/item/list) — see `yshopping_get_reviews`. Reviews past the first 20 and the page's sort orders (recommended, highest rated, most helpful, newest, pick-up) go through the same continuation endpoint the page itself uses, with the token the page issued; choose them with `offset` and `sort`. The page's star-rating and aspect filters are available as `rating` and `sub_review_id` through the same endpoint, and the response includes the per-aspect breakdown (`subReviewSummaries`). Suggestions (`yshopping_suggest_keywords`) reproduce the app's suggest API and were added in v0.4.0.
- **Amazon.co.jp**: the app has no native search API. Its WebView display is reproduced with plain HTTP requests, using the app-format user agent and the cookies the app syncs before navigation; no browser engine is involved. This service also does not respond to non-Japan-side access.

## Notes on the providers added in v0.3.0

- **YouTube**: reproduces the Android app's own retrieval route (InnerTube). Search supports sorting (relevance, rating, upload date, view count) and filtering (upload window, duration), with continuation-token paging, and returns non-video results (channels, playlists, Shorts) with their type. Video details, related videos (with continuation for the next page), comments (with paging) and caption text (`youtube_get_captions`) are also available. The guest identifier (visitorData) is the server-issued value, held and replayed as the app does — no login. Video details include the exact view count, the like count and the relative upload time (e.g. "5 years ago") from the app's updated-metadata surface. A calendar publish date exists nowhere in the Android app's responses, so it stays null.
- **Bilibili**: reproduces the Android app's gRPC. Search (SearchAll) supports the order code, the region (tid) and duration filter formats, a publish-time range, and `next`-token paging. Video detail (View) and related videos (RelatesFeed) are also available: the gRPC `-bin` metadata is sent base64-encoded, which is the wire convention the public edge accepts (raw binary metadata is rejected by the edge).
- **CSDN**: reproduces the Android app's gateway API with the app's own X-Ca signature (key embedded in the app). Article search (with paging) and article detail (including the full body HTML and text) work without login. Search supports the app's categories (all / blog / ask / download / course / mall / ebook / user / vip / special columns), sorting, per-category filters and per-author personal-home search. There is no forum search: the app itself has none.

## Added and extended in the v0.4.0 audit

This release integrates a full audit of incomplete implementations (140 inventoried items) across every provider. Highlights:

- **Baidu**: three-system suggestions (`baidu_suggest`) and page-number addressing for `baidu_search_page`.
- **Bing**: shopping search, trending topics and visual (image) search. Trending and visual search are implemented but Bing answers anonymous callers with 401 in live checks.
- **Naver**: cafe search and KIN (지식iN) search.
- **Yandex**: suggest endings (`yandex_suggest_endings`) and special search cards.
- **AliExpress**: price/switch search filters, review filter/sort, SKU query/adjust and store tools. `aliexpress_query_sku` is live-verified; the other new tools are implemented to the app's request format and marked as not live-verified in their descriptions.
- **Goofish**: search filters (attributes, price range, region, category), page-state chaining for recommendations and the home feed, similar items, category guide, ratings, seller info, city feeds, fishponds and video playback info. Most new tools follow the app's API format and are not yet live-verified.
- **Mercari**: the full search-condition set (coupons, attributes, SKU/shop ids, shipping origin and more).
- **Yahoo! Auctions**: the full search-parameter set, statistics (open/closed) and seller/store profiles.
- **Rakuten Ichiba**: suggestions, product-aggregated search, the genre tree and variant details.
- **Qoo10**: search-filter metadata (`qoo10_get_search_filters`) — the brand/color/price codes come from here.
- **Amazon.co.jp**: suggestions and ASIN-addressed search. Reviews are not offered: they require sign-in.

## Install

Download the ZIP for your OS from Releases and extract it.

1. Run `zero-search-mcp-launcher` and click Install for your agent, or
2. Run `zero-search-mcp setup` in a terminal and paste the JSON it prints into your MCP config.

The config is just this:

```json
{
  "mcpServers": {
    "ZeroSearch_MCP": { "command": "/path/to/zero-search-mcp", "args": [] }
  }
}
```

Goofish is not an exception. The distribution bundles a Rust signing harness that runs the app's own SecurityGuard native library under the Unicorn engine, plus the Android system libraries it needs (about 7 MB in total; no Java is required). On first Goofish use, the server downloads the Goofish app package (7.28.70, ~269 MB) from the app store's CDN by itself, verifies its SHA-256, and assembles the signing runtime in its own data directory; the device identity is likewise generated by the server itself. There is nothing to install or configure. Only the first call can take several minutes to tens of minutes (the download plus the signing runtime's boot). Budget roughly 300 MB of disk in total (bundled payload + app package + signing working files). See [runtime/goofish](runtime/goofish/README.md) for how it works.

Yahoo (US) image search follows the same pattern. Yahoo's image host returns HTTP 500 to plain HTTP clients, so this one vertical is fetched by the bundled helper process `zero-search-yahoo-us-image-helper` (placed next to the server binary), which requests the pages with a browser-grade client. No configuration is needed.

## Use as a library

The `zero_search` crate is the unified API. Every provider crate is re-exported, so the raw clients and types stay reachable too.

```toml
[dependencies]
zero-search = { git = "https://github.com/mouseos/ZeroSearch_MCP" }
tokio = { version = "1", features = ["full"] }
```

```rust
use zero_search::{Client, WebEngine, ShopProvider, Category};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let client = Client::new()?; // reads no configuration

    // One engine (results normalised into SearchItem)
    let items = client.search(WebEngine::Bing, "rust programming").await?;

    // All web engines at once (success/failure reported per engine)
    let outcomes = client.search_all("tokyo weather").await;
    for outcome in &outcomes {
        println!("{}: ok={} items={}", outcome.engine.id(), outcome.ok, outcome.items.len());
    }

    // A whole category at once
    let shopping = client.search_category(Category::Shopping, "headphones").await;

    // Product search (returns the provider's own JSON)
    let value = client.search_products(ShopProvider::Mercari, "camera", 10).await?;

    Ok(())
}
```

### Embedding in a harness

The intended shape is to expose the unified API as your harness's search tool directly. A runnable example lives at [crates/facade/examples/harness_tool.rs](crates/facade/examples/harness_tool.rs):

```sh
cargo run -p zero-search --example harness_tool -- "tokyo weather"
cargo run -p zero-search --example harness_tool -- --shop "headphones"
```

## Migrating from the old repositories

ZeroSearch merges these seven repositories, which are now archived:

- mouseos/Baidu_MCP
- mouseos/Yahoo-Japan-Search-MCP
- mouseos/Bing_MCP
- mouseos/Naver_MCP
- mouseos/Yahoo_US_MCP
- mouseos/Yandex_MCP
- mouseos/Shopping_MCP

To migrate, change the server path in your MCP config to `zero-search-mcp`. Tool names and arguments did not change, so existing prompts and workflows keep working. Library crate names (`baidu_search`, `bing_search`, ...) and their types are preserved as well.

## Development

```sh
cargo build --workspace
cargo test --workspace -- --skip e2e_     # offline tests
cargo test -p zero-search-mcp-server -- e2e_ --test-threads=1   # live (serial only)
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

## License

MIT
