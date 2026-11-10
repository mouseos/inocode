//! Harness integration example: expose the ZeroSearch unified API
//! as a tool of your own agent harness.
//!
//! This is the intended embedding shape. A harness defines one tool
//! (here: a plain async function returning JSON) whose
//! implementation is a thin wrapper over `zero_search`:
//!
//! - `web_search(query)` — every web engine at once, per-engine
//!   outcomes kept separate;
//! - `web_search_engine(engine, query)` — one engine;
//! - `product_search(provider, query)` — one shopping provider.
//!
//! No API key, no environment variables, no configuration: the
//! harness only constructs `zero_search::Client` once and reuses it.
//!
//! Run it (performs live searches):
//!   cargo run -p zero-search --example harness_tool -- "tokyo weather"
//!   cargo run -p zero-search --example harness_tool -- --engine bing "rust"
//!   cargo run -p zero-search --example harness_tool -- --shop "headphones"

use zero_search::{Client, ShopProvider, WebEngine};

/// The harness-side tool surface. In a real harness each function
/// would be registered as a tool with the JSON schema sketched in
/// the comments; the bodies are the whole integration.
struct SearchTools {
    client: Client,
}

impl SearchTools {
    fn new() -> anyhow::Result<Self> {
        Ok(Self {
            client: Client::new()?,
        })
    }

    /// Tool: web_search { query: string }
    /// Search all web engines; returns one outcome per engine.
    async fn web_search(&self, query: &str) -> serde_json::Value {
        let outcomes = self.client.search_all(query).await;
        serde_json::json!({ "query": query, "engines": outcomes })
    }

    /// Tool: web_search_engine { engine: "baidu"|"yahoo_jp"|"bing"|
    /// "naver"|"yahoo_us"|"yandex", query: string }
    async fn web_search_engine(&self, engine: &str, query: &str) -> serde_json::Value {
        let Some(engine) = WebEngine::all().iter().find(|e| e.id() == engine) else {
            return serde_json::json!({ "ok": false, "error": format!("unknown engine: {engine}") });
        };
        match self.client.search(*engine, query).await {
            Ok(items) => serde_json::json!({ "ok": true, "engine": engine.id(), "items": items }),
            Err(e) => {
                serde_json::json!({ "ok": false, "engine": engine.id(), "error": format!("{e:#}") })
            }
        }
    }

    /// Tool: product_search { provider: "aliexpress"|"goofish"|
    /// "mercari"|"yahoo_auctions"|"yahoo_fleamarket"|"rakuma",
    /// query: string }
    async fn product_search(&self, provider: &str, query: &str) -> serde_json::Value {
        let Some(provider) = ShopProvider::all().iter().find(|p| p.id() == provider) else {
            return serde_json::json!({ "ok": false, "error": format!("unknown provider: {provider}") });
        };
        match self.client.search_products(*provider, query, 5).await {
            Ok(result) => result,
            Err(e) => serde_json::json!({ "ok": false, "error": format!("{e:#}") }),
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let tools = SearchTools::new()?;
    let out = match args.first().map(String::as_str) {
        Some("--shop") => {
            let query = args.get(1).cloned().unwrap_or_else(|| "headphones".into());
            tools.product_search("mercari", &query).await
        }
        Some("--engine") => {
            let engine = args.get(1).cloned().unwrap_or_else(|| "bing".into());
            let query = args.get(2).cloned().unwrap_or_else(|| "rust".into());
            tools.web_search_engine(&engine, &query).await
        }
        Some(q) => tools.web_search(q).await,
        None => tools.web_search("tokyo weather").await,
    };
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}
