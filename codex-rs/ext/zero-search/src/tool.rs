use std::sync::Arc;

use codex_extension_api::ExtensionTurnItem;
use codex_extension_api::ResponsesApiTool;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolExecutor;
use codex_extension_api::ToolName;
use codex_extension_api::ToolOutput;
use codex_extension_api::ToolPayload;
use codex_extension_api::ToolSpec;
use codex_extension_api::parse_tool_input_schema_without_compaction;
use codex_extension_items::ExtensionItem;
use codex_extension_items::web_search::WebSearchAction;
use codex_extension_items::web_search::WebSearchItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::models::WebSearchAction as CoreWebSearchAction;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::WebSearchBeginEvent;
use codex_protocol::protocol::WebSearchEndEvent;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;
use zero_search::ArticleProvider;
use zero_search::Client;
use zero_search::EngineOutcome;
use zero_search::ErrorKind;
use zero_search::MediaProvider;
use zero_search::ShopProvider;
use zero_search::WebEngine;

use crate::format::format_web_results;
use crate::format::truncate_output;

const DEFAULT_WEB_LIMIT: usize = 10;
const MAX_WEB_LIMIT: usize = 30;
const DEFAULT_ITEM_LIMIT: i64 = 5;
const MAX_ITEM_LIMIT: i64 = 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ToolKind {
    Web,
    Shopping,
    Media,
}

pub(crate) struct ZeroSearchTool {
    kind: ToolKind,
    client: Arc<Client>,
}

impl ZeroSearchTool {
    pub(crate) fn new(kind: ToolKind, client: Arc<Client>) -> Self {
        Self { kind, client }
    }

    fn name(&self) -> &'static str {
        match self.kind {
            ToolKind::Web => "web_search",
            ToolKind::Shopping => "shopping_search",
            ToolKind::Media => "media_search",
        }
    }
}

fn ids<T>(all: &[T], id: impl Fn(&T) -> &'static str) -> Vec<&'static str> {
    all.iter().map(id).collect()
}

fn media_ids() -> Vec<&'static str> {
    let mut ids = ids(MediaProvider::all(), MediaProvider::id);
    ids.extend(self::ids(ArticleProvider::all(), ArticleProvider::id));
    ids
}

pub(crate) fn schema(kind: ToolKind) -> Value {
    match kind {
        ToolKind::Web => {
            let mut engines = vec!["all"];
            engines.extend(ids(WebEngine::all(), WebEngine::id));
            json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "What to search for."},
                    "engine": {
                        "type": "string",
                        "enum": engines,
                        "description": "Search engine. Defaults to all: every engine is asked and the results are merged. Engines sometimes refuse a request (captcha, rate limit); the reply names the ones that failed."
                    },
                    "limit": {"type": "integer", "description": "Maximum number of results. Defaults to 10, at most 30."}
                },
                "required": ["query"],
                "additionalProperties": false
            })
        }
        ToolKind::Shopping => json!({
            "type": "object",
            "properties": {
                "provider": {
                    "type": "string",
                    "enum": ids(ShopProvider::all(), ShopProvider::id),
                    "description": "Shopping site to search."
                },
                "query": {"type": "string", "description": "Product to search for."},
                "limit": {"type": "integer", "description": "Maximum number of products. Defaults to 5, at most 20."}
            },
            "required": ["provider", "query"],
            "additionalProperties": false
        }),
        ToolKind::Media => json!({
            "type": "object",
            "properties": {
                "provider": {
                    "type": "string",
                    "enum": media_ids(),
                    "description": "Video or article site to search."
                },
                "query": {"type": "string", "description": "What to search for."},
                "limit": {"type": "integer", "description": "Maximum number of results. Defaults to 5, at most 20."}
            },
            "required": ["provider", "query"],
            "additionalProperties": false
        }),
    }
}

fn description(kind: ToolKind) -> &'static str {
    match kind {
        ToolKind::Web => {
            "Searches the web with public search engines (no account or API key needed) and returns titles, URLs and snippets. Use it for current information. Pick the engine that fits the query: yahoo_jp for Japanese, baidu for Chinese, naver for Korean, yandex for Russian, bing or yahoo_us for English and general queries. Omit engine to ask all engines and merge the results. To read a page, fetch its URL with a command."
        }
        ToolKind::Shopping => {
            "Searches an online shopping or marketplace site (AliExpress, Mercari, Yahoo Auctions, Rakuten, Amazon and others) and returns the products found as JSON."
        }
        ToolKind::Media => {
            "Searches YouTube, Bilibili or CSDN articles and returns the matching videos or articles as JSON."
        }
    }
}

impl<'call> ToolExecutor<ToolCall<'call>> for ZeroSearchTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(self.name())
    }

    fn spec(&self) -> ToolSpec {
        let parameters = match parse_tool_input_schema_without_compaction(&schema(self.kind)) {
            Ok(parameters) => parameters,
            Err(err) => panic!("search tool schema should parse: {err}"),
        };
        ToolSpec::Function(ResponsesApiTool {
            name: self.name().to_string(),
            description: description(self.kind).to_string(),
            strict: false,
            parameters,
            output_schema: None,
            defer_loading: None,
        })
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn handle<'a>(&'a self, call: ToolCall<'call>) -> codex_extension_api::ToolExecutorFuture<'a>
    where
        'call: 'a,
    {
        Box::pin(async move {
            let result = match self.kind {
                ToolKind::Web => self.web(&call).await,
                ToolKind::Shopping => self.shopping(&call).await,
                ToolKind::Media => self.media(&call).await,
            };
            Ok(Box::new(SearchOutput::new(result)) as Box<dyn ToolOutput>)
        })
    }
}

#[derive(Deserialize)]
struct WebArgs {
    query: String,
    #[serde(default)]
    engine: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Deserialize)]
struct ProviderArgs {
    provider: String,
    query: String,
    #[serde(default)]
    limit: Option<i64>,
}

fn parse<T: for<'de> Deserialize<'de>>(call: &ToolCall<'_>) -> Result<T, String> {
    let arguments = call.function_arguments().map_err(|err| err.to_string())?;
    serde_json::from_str(arguments).map_err(|err| format!("invalid arguments: {err}"))
}

fn require_query(query: &str) -> Result<&str, String> {
    let query = query.trim();
    if query.is_empty() {
        Err("query must not be empty".to_string())
    } else {
        Ok(query)
    }
}

fn item_limit(limit: Option<i64>) -> i64 {
    limit.unwrap_or(DEFAULT_ITEM_LIMIT).clamp(1, MAX_ITEM_LIMIT)
}

impl ZeroSearchTool {
    async fn web(&self, call: &ToolCall<'_>) -> Result<String, String> {
        let args: WebArgs = parse(call)?;
        let query = require_query(&args.query)?.to_string();
        let limit = args
            .limit
            .unwrap_or(DEFAULT_WEB_LIMIT)
            .clamp(1, MAX_WEB_LIMIT);
        let engine = match args.engine.as_deref() {
            None | Some("all") => None,
            Some(id) => Some(
                WebEngine::all()
                    .iter()
                    .find(|engine| engine.id() == id)
                    .copied()
                    .ok_or_else(|| format!("unknown engine {id:?}"))?,
            ),
        };

        call.turn_item_emitter
            .emit_started(web_item(call, "", None, None, EventMsg::WebSearchBegin(
                WebSearchBeginEvent {
                    call_id: call.call_id.clone(),
                },
            )))
            .await;
        let outcomes = match engine {
            None => self.client.search_all(&query).await,
            Some(engine) => vec![single_engine(&self.client, engine, &query).await],
        };
        let (text, any) = format_web_results(&query, &outcomes, limit);
        let results: Vec<Value> = crate::format::merge_items(&outcomes, limit)
            .into_iter()
            .filter_map(|item| serde_json::to_value(item).ok())
            .collect();
        call.turn_item_emitter
            .emit_completed(web_item(
                call,
                &query,
                Some(WebSearchAction::Search {
                    query: Some(query.clone()),
                    queries: None,
                }),
                Some(results.clone()),
                EventMsg::WebSearchEnd(WebSearchEndEvent {
                    call_id: call.call_id.clone(),
                    query: query.clone(),
                    action: CoreWebSearchAction::Search {
                        query: Some(query.clone()),
                        queries: None,
                    },
                    results: Some(results),
                }),
            ))
            .await;
        if any { Ok(text) } else { Err(text) }
    }

    async fn shopping(&self, call: &ToolCall<'_>) -> Result<String, String> {
        let args: ProviderArgs = parse(call)?;
        let query = require_query(&args.query)?;
        let provider = ShopProvider::all()
            .iter()
            .find(|provider| provider.id() == args.provider)
            .copied()
            .ok_or_else(|| format!("unknown provider {:?}", args.provider))?;
        let value = self
            .client
            .search_products(provider, query, item_limit(args.limit))
            .await
            .map_err(|err| failure_text(&format!("{err:#}")))?;
        json_result(&value)
    }

    async fn media(&self, call: &ToolCall<'_>) -> Result<String, String> {
        let args: ProviderArgs = parse(call)?;
        let query = require_query(&args.query)?;
        let limit = item_limit(args.limit);
        let value = if let Some(provider) = MediaProvider::all()
            .iter()
            .find(|provider| provider.id() == args.provider)
        {
            self.client.search_media(*provider, query, limit).await
        } else if let Some(provider) = ArticleProvider::all()
            .iter()
            .find(|provider| provider.id() == args.provider)
        {
            self.client.search_articles(*provider, query, limit).await
        } else {
            return Err(format!("unknown provider {:?}", args.provider));
        }
        .map_err(|err| failure_text(&format!("{err:#}")))?;
        json_result(&value)
    }
}

/// A provider reports refusals inside its JSON payload with `ok: false`; treat those as failures.
fn json_result(value: &Value) -> Result<String, String> {
    let text = truncate_output(value.to_string());
    if value.get("ok").and_then(Value::as_bool) == Some(false) {
        Err(text)
    } else {
        Ok(text)
    }
}

fn failure_text(message: &str) -> String {
    let kind = serde_json::to_value(ErrorKind::from_error_message(message))
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string());
    truncate_output(format!("Search failed ({kind}): {message}"))
}

async fn single_engine(client: &Client, engine: WebEngine, query: &str) -> EngineOutcome {
    match client.search(engine, query).await {
        Ok(items) => EngineOutcome {
            engine,
            ok: true,
            items,
            error: None,
            error_kind: None,
        },
        Err(err) => {
            let message = format!("{err:#}");
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

fn web_item(
    call: &ToolCall<'_>,
    query: &str,
    action: Option<WebSearchAction>,
    results: Option<Vec<Value>>,
    legacy_event: EventMsg,
) -> ExtensionTurnItem {
    ExtensionTurnItem {
        item: ExtensionItem::WebSearch(WebSearchItem {
            id: call.call_id.clone(),
            query: query.to_string(),
            action,
            results,
        }),
        legacy_events: vec![legacy_event],
    }
}

struct SearchOutput {
    payload: FunctionCallOutputPayload,
}

impl SearchOutput {
    fn new(result: Result<String, String>) -> Self {
        let payload = match result {
            Ok(text) => FunctionCallOutputPayload::from_text(text),
            Err(error) => FunctionCallOutputPayload {
                success: Some(false),
                ..FunctionCallOutputPayload::from_text(error)
            },
        };
        Self { payload }
    }
}

impl ToolOutput for SearchOutput {
    fn log_output(&self) -> String {
        "[built-in search output]".to_string()
    }

    fn success_for_logging(&self) -> bool {
        self.payload.success.unwrap_or(true)
    }

    fn contains_external_context(&self) -> bool {
        true
    }

    fn to_response_item(&self, call_id: &str, _payload: &ToolPayload) -> ResponseInputItem {
        ResponseInputItem::FunctionCallOutput {
            call_id: call_id.to_string(),
            output: self.payload.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use serde_json::json;

    use super::*;

    #[test]
    fn schemas_list_every_provider_the_library_knows() {
        let engines = schema(ToolKind::Web)["properties"]["engine"]["enum"].clone();
        assert_eq!(
            engines,
            json!(["all", "baidu", "yahoo_jp", "bing", "naver", "yahoo_us", "yandex"])
        );
        let shops = schema(ToolKind::Shopping)["properties"]["provider"]["enum"]
            .as_array()
            .map(Vec::len);
        assert_eq!(shops, Some(ShopProvider::all().len()));
        assert_eq!(
            schema(ToolKind::Media)["properties"]["provider"]["enum"],
            json!(["youtube", "bilibili", "csdn"])
        );
    }

    #[test]
    fn every_tool_spec_parses() {
        let client = Arc::new(Client::new().expect("client"));
        for kind in [ToolKind::Web, ToolKind::Shopping, ToolKind::Media] {
            let tool = ZeroSearchTool::new(kind, Arc::clone(&client));
            let ToolSpec::Function(spec) = ToolExecutor::<ToolCall<'_>>::spec(&tool) else {
                panic!("expected a function tool");
            };
            assert_eq!(spec.name, tool.name());
        }
    }

    #[test]
    fn limits_and_queries_are_validated() {
        assert_eq!(item_limit(None), 5);
        assert_eq!(item_limit(Some(0)), 1);
        assert_eq!(item_limit(Some(999)), 20);
        assert!(require_query("   ").is_err());
        assert_eq!(require_query(" rust "), Ok("rust"));
    }

    #[test]
    fn provider_refusals_are_reported_as_failures() {
        assert!(json_result(&json!({"ok": true, "items": []})).is_ok());
        assert!(json_result(&json!({"ok": false, "error": "captcha"})).is_err());
        assert!(failure_text("HTTP 429 too many requests").contains("rate_limited"));
    }
}
