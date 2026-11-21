//! The models a provider lists, with their list prices (models.dev), their
//! benchmark scores (LLMBenchmark) and the speed measured here, for picking
//! a model. Design and sources: docs/evidence/model-catalog.md.

use std::path::Path;
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};

use crate::usage_log::ModelStats;
use crate::ModelInfo;

pub(crate) const PRICES_URL: &str = "https://models.dev/api.json";
pub(crate) const SCORES_URL: &str = "https://api.llmbenchmark.ai/v1/scores";
const PRICES_TTL: Duration = Duration::from_secs(24 * 3_600);
const SCORES_TTL: Duration = Duration::from_secs(12 * 3_600);
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_BODY: usize = 64 * 1024 * 1024;

/// Which benchmark metrics answer which use, as the earlier InoCode chose
/// them (docs/evidence/model-catalog.md).
pub(crate) const USES: [(&str, &[&str]); 6] = [
    (
        "coding",
        &[
            "livebench_coding",
            "vals_vibe_code",
            "tbench_4_0_accuracy",
            "scale_swe_bench_pro_v2",
            "cursorbench_4_0_score",
            "epoch_frontierswe",
        ],
    ),
    (
        "reasoning",
        &["livebench_reasoning", "epoch_gpqa_diamond", "epoch_arc_agi"],
    ),
    (
        "math",
        &[
            "epoch_otis_mock_aime",
            "epoch_frontiermath",
            "epoch_proofbench",
        ],
    ),
    (
        "agentic",
        &[
            "livebench_long_agentic",
            "livebench_browser",
            "epoch_apex_agents",
        ],
    ),
    ("general", &["aa_intelligence_index"]),
    (
        "speed",
        &["or_output_tokens_per_second", "or_time_to_first_token_s"],
    ),
];

/// The catalog the person's screen shows.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Catalog {
    pub models: Vec<Entry>,
    /// The account's plan, when the provider says.
    pub plan: Option<String>,
    pub sources: Vec<Value>,
    /// Some data is from an old copy because a fetch failed.
    pub stale: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Entry {
    pub id: String,
    pub display_name: Option<String>,
    /// `api` (paid per token with a key), `plan_included` (a subscription),
    /// `theos` (DeepAI's own unit) or `free`.
    pub billing: &'static str,
    pub price: Option<Price>,
    /// DeepAI's Theos per request.
    pub theos_per_request: Option<String>,
    pub scores: Vec<Score>,
    pub measured: Option<Measured>,
    pub context_window: Option<u64>,
    /// Speed tiers the provider offers besides the standard one (`fast`,
    /// `flex`).
    pub speed_tiers: Option<Vec<String>>,
    /// Access programs the model is offered under (OpenAI's cyber programs
    /// such as Daybreak).
    pub access_programs: Option<Vec<String>>,
    /// The badge Claude's own model list gives the model for this account,
    /// as Claude sends it; it tells the Cyber Verification Program access
    /// (for example the message "CVP: Defense Access").
    pub badge: Option<Value>,
}

/// List prices in US dollars per million tokens.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Price {
    pub input: f64,
    pub output: f64,
    pub cache_read: Option<f64>,
    pub cache_write: Option<f64>,
    /// The models.dev entry it was taken from, `provider/model`.
    pub matched: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Score {
    #[serde(rename = "use")]
    pub use_: &'static str,
    pub metric: String,
    pub value: f64,
    pub unit: String,
    /// 1 is the best among the `of` models the metric lists.
    pub rank: usize,
    pub of: usize,
    pub observed_at: Option<String>,
    pub source_url: Option<String>,
}

/// What the requests logged here say about the model.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Measured {
    pub requests: u64,
    pub tokens_per_second: Option<f64>,
    pub first_output_ms: Option<u64>,
    pub cache_share: Option<f64>,
}

/// The data sources and their licenses, shown with the catalog.
pub(crate) fn sources() -> Vec<Value> {
    vec![
        json!({"name": "models.dev", "url": "https://models.dev", "license": "MIT",
               "used_for": "list prices"}),
        json!({"name": "LLMBenchmark", "url": "https://llmbenchmark.ai", "license": "CC BY 4.0",
               "used_for": "benchmark scores",
               "attribution": "Benchmark scores from LLMBenchmark (llmbenchmark.ai), CC BY 4.0"}),
    ]
}

/// A copy of `url` kept in `folder` as `key`, fetched again once older
/// than `ttl`; an old copy is used, marked stale, when the fetch fails.
pub(crate) async fn cached(
    client: &reqwest::Client,
    folder: &Path,
    key: &str,
    url: &str,
    ttl: Duration,
) -> (Option<Value>, bool) {
    let path = folder.join(format!("{key}.json"));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |now| now.as_secs());
    let kept: Option<Value> = std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok());
    let fetched_at = kept
        .as_ref()
        .and_then(|kept| kept.get("fetched_at"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if kept.is_some() && now.saturating_sub(fetched_at) < ttl.as_secs() {
        return (kept.and_then(|kept| kept.get("data").cloned()), false);
    }
    match fetch(client, url).await {
        Some(data) => {
            let copy = json!({"fetched_at": now, "data": data});
            let _ = std::fs::create_dir_all(folder);
            let temporary = folder.join(format!("{key}.json.tmp"));
            if std::fs::write(&temporary, copy.to_string()).is_ok() {
                let _ = std::fs::rename(&temporary, &path);
            }
            (Some(data), false)
        }
        None => (kept.and_then(|kept| kept.get("data").cloned()), true),
    }
}

async fn fetch(client: &reqwest::Client, url: &str) -> Option<Value> {
    let mut response = client
        .get(url)
        .timeout(FETCH_TIMEOUT)
        .header("user-agent", "inocode-model-catalog")
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let body = crate::llm::read_capped(&mut response, MAX_BODY)
        .await
        .ok()??;
    serde_json::from_slice(&body).ok()
}

/// Lower case, with `.` read as `-` (`gpt-5.2` is `gpt-5-2`), without a
/// `provider/` prefix.
fn normalized(id: &str) -> String {
    let id = id.rsplit('/').next().unwrap_or(id);
    id.trim().to_ascii_lowercase().replace('.', "-")
}

/// The id without a date at its end (`-20251001`, `-1017`).
fn without_date(id: &str) -> &str {
    match id.rsplit_once('-') {
        Some((head, tail)) if tail.len() >= 4 && tail.chars().all(|c| c.is_ascii_digit()) => head,
        _ => id,
    }
}

/// The catalog providers a model id belongs to, by its prefix.
fn providers_of(id: &str) -> &'static [&'static str] {
    let id = normalized(id);
    let table: [(&[&str], &'static [&'static str]); 12] = [
        (&["claude"], &["anthropic"]),
        (&["gpt", "o1", "o3", "o4", "chatgpt", "codex"], &["openai"]),
        (&["gemini", "gemma"], &["google"]),
        (&["glm"], &["zai", "z-ai", "zhipuai"]),
        (&["deepseek"], &["deepseek"]),
        (&["kimi"], &["moonshotai", "moonshot"]),
        (&["qwen"], &["qwen", "alibaba"]),
        (&["grok"], &["xai", "x-ai"]),
        (&["muse"], &["meta"]),
        (
            &["mistral", "devstral", "codestral", "magistral"],
            &["mistral"],
        ),
        (&["minimax"], &["minimax"]),
        (&["mimo"], &["xiaomi"]),
    ];
    table
        .iter()
        .find(|(prefixes, _)| prefixes.iter().any(|prefix| id.starts_with(prefix)))
        .map_or(&[], |(_, providers)| providers)
}

/// The list price of `id` in a models.dev catalog; `None` when it is not
/// listed or listed at 0, which subscription providers write for "not
/// sold per token".
pub(crate) fn price(catalog: &Value, id: &str) -> Option<Price> {
    let wanted = [normalized(id), normalized(without_date(id))];
    for provider in providers_of(id) {
        let Some(models) = catalog
            .get(provider)?
            .get("models")
            .and_then(Value::as_object)
        else {
            continue;
        };
        let found = models
            .iter()
            .find(|(key, _)| wanted.contains(&normalized(key)));
        if let Some((key, model)) = found {
            let cost = model.get("cost")?;
            let rate = |name: &str| cost.get(name).and_then(Value::as_f64);
            let (input, output) = (rate("input")?, rate("output")?);
            if input == 0.0 && output == 0.0 {
                return None;
            }
            return Some(Price {
                input,
                output,
                cache_read: rate("cache_read"),
                cache_write: rate("cache_write"),
                matched: format!("{provider}/{key}"),
            });
        }
    }
    None
}

/// The scores of `id` in the rows of each metric, ranked among the rows.
pub(crate) fn scores(boards: &[(String, Vec<Value>)], id: &str) -> Vec<Score> {
    let wanted = [normalized(id), normalized(without_date(id))];
    let providers = providers_of(id);
    let mut found = Vec::new();
    for (metric, rows) in boards {
        let Some(use_) = USES
            .iter()
            .find(|(_, metrics)| metrics.contains(&metric.as_str()))
            .map(|(use_, _)| *use_)
        else {
            continue;
        };
        let value_of = |row: &Value| {
            row.get("value")
                .and_then(Value::as_f64)
                .filter(|v| v.is_finite())
        };
        let matches = |row: &Value| {
            let model = row.get("modelId").and_then(Value::as_str).unwrap_or("");
            let provider = model
                .split_once('/')
                .map(|(provider, _)| provider.to_ascii_lowercase());
            wanted.contains(&normalized(model))
                && provider.is_none_or(|provider| providers.contains(&provider.as_str()))
        };
        let Some(row) = rows
            .iter()
            .find(|row| matches(row) && value_of(row).is_some())
        else {
            continue;
        };
        let value = value_of(row).expect("checked above");
        let lower_is_better = metric.contains("time") || metric.ends_with("_s");
        let better = rows
            .iter()
            .filter_map(value_of)
            .filter(|other| {
                if lower_is_better {
                    *other < value
                } else {
                    *other > value
                }
            })
            .count();
        let text = |key: &str| row.get(key).and_then(Value::as_str).map(str::to_string);
        found.push(Score {
            use_,
            metric: metric.clone(),
            value,
            unit: text("unit").unwrap_or_default(),
            rank: better + 1,
            of: rows.iter().filter(|row| value_of(row).is_some()).count(),
            observed_at: text("observedAt"),
            source_url: text("sourceUrl"),
        });
    }
    found
}

/// Days of the request log the measured speed is taken from.
const MEASURED_DAYS: u64 = 30;

/// The catalog of `models`, the list `llm`'s provider gave: prices and
/// scores fetched (or kept from an earlier fetch in the store's folder),
/// the speed measured from the request log.
pub(crate) async fn build(
    llm: &crate::Llm,
    store: &crate::store::Store,
    models: Vec<ModelInfo>,
) -> Catalog {
    let folder = store.files_folder().join("catalog");
    let client = llm.http();
    let (prices, mut stale) = cached(client, &folder, "models-dev", PRICES_URL, PRICES_TTL).await;
    let mut boards = Vec::new();
    for metric in USES.iter().flat_map(|(_, metrics)| metrics.iter()) {
        let url = format!("{SCORES_URL}?metric={metric}");
        let (rows, old) = cached(
            client,
            &folder,
            &format!("scores-{metric}"),
            &url,
            SCORES_TTL,
        )
        .await;
        stale |= old;
        if let Some(Value::Array(rows)) = rows {
            boards.push((metric.to_string(), rows));
        }
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |now| now.as_millis() as u64);
    let since = now.saturating_sub(MEASURED_DAYS * 24 * 3_600_000);
    let stats = store.model_stats(since, 0).await.unwrap_or_default();
    let provider = llm.provider_label();
    let billing = llm.billing();
    let mut badges = llm.claude_badges().await;
    let models = models
        .iter()
        .map(|model| {
            let measured = stats
                .iter()
                .find(|stats| stats.provider == provider && stats.model == model.id);
            let mut entry = entry(model, billing, prices.as_ref(), &boards, measured);
            entry.badge = badges.remove(&model.id);
            entry
        })
        .collect();
    Catalog {
        models,
        plan: llm.claude_plan().await,
        sources: sources(),
        stale,
    }
}

/// The catalog entry of one listed model.
pub(crate) fn entry(
    model: &ModelInfo,
    billing: &'static str,
    prices: Option<&Value>,
    boards: &[(String, Vec<Value>)],
    measured: Option<&ModelStats>,
) -> Entry {
    let free = model
        .theos_per_request
        .as_deref()
        .is_some_and(|rate| rate.trim_start_matches(['0', '.']).is_empty());
    let billing = match billing {
        "theos" if free => "free",
        other => other,
    };
    Entry {
        id: model.id.clone(),
        display_name: model.display_name.clone(),
        billing,
        // DeepAI bills in Theos, so a list price would mislead.
        price: match billing {
            "theos" | "free" => None,
            _ => prices.and_then(|prices| price(prices, &model.id)),
        },
        theos_per_request: model.theos_per_request.clone(),
        scores: scores(boards, &model.id),
        measured: measured.map(|stats| Measured {
            requests: stats.requests,
            tokens_per_second: stats.tokens_per_second,
            first_output_ms: stats.first_output_ms,
            cache_share: stats.cache_share,
        }),
        context_window: model.context_window,
        speed_tiers: model.speed_tiers.clone(),
        access_programs: model.access_programs.clone(),
        badge: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(id: &str) -> ModelInfo {
        ModelInfo {
            id: id.into(),
            display_name: None,
            owned_by: None,
            reasoning_levels: None,
            default_reasoning: None,
            context_window: None,
            max_output: None,
            speed_tiers: None,
            access_programs: None,
            theos_per_request: None,
        }
    }

    #[test]
    fn prices_are_found_by_normalized_id_and_zero_is_not_a_price() {
        let catalog = json!({
            "anthropic": {"models": {
                "claude-haiku-5-5": {"cost": {"input": 1.0, "output": 5.0, "cache_read": 0.1}},
                "claude-free": {"cost": {"input": 0, "output": 0}}
            }},
            "openai": {"models": {"gpt-5.2": {"cost": {"input": 2.5, "output": 10}}}}
        });
        let haiku = price(&catalog, "claude-haiku-5-5-20261001").expect("dated id");
        assert_eq!(
            (haiku.input, haiku.output, haiku.cache_read),
            (1.0, 5.0, Some(0.1))
        );
        assert_eq!(haiku.matched, "anthropic/claude-haiku-5-5");
        assert_eq!(
            price(&catalog, "gpt-5-2").expect("dots").matched,
            "openai/gpt-5.2"
        );
        assert_eq!(price(&catalog, "claude-free"), None);
        assert_eq!(price(&catalog, "unknown-model"), None);
    }

    #[test]
    fn scores_are_ranked_among_the_metric_rows() {
        let boards = vec![
            (
                "aa_intelligence_index".to_string(),
                vec![
                    json!({"modelId": "openai/gpt-6.2-astra", "value": 62.1, "unit": "points"}),
                    json!({"modelId": "google/gemini-4-neon", "value": 61.4, "unit": "points"}),
                    json!({"modelId": "anthropic/gpt-6.2-astra", "value": 99.0}),
                ],
            ),
            (
                "or_time_to_first_token_s".to_string(),
                vec![
                    json!({"modelId": "openai/gpt-6.2-astra", "value": 2.0}),
                    json!({"modelId": "google/gemini-4-neon", "value": 1.0}),
                ],
            ),
            (
                "unknown_metric".to_string(),
                vec![json!({"modelId": "openai/gpt-6.2-astra", "value": 1})],
            ),
        ];
        let found = scores(&boards, "gpt-6.2-astra");
        assert_eq!(found.len(), 2);
        assert_eq!(
            (found[0].use_, found[0].value, found[0].rank, found[0].of),
            ("general", 62.1, 2, 3)
        );
        // Lower is better for times.
        assert_eq!((found[1].use_, found[1].rank), ("speed", 2));
    }

    #[test]
    fn deepai_models_bill_in_theos_and_show_no_list_price() {
        let prices =
            json!({"deepseek": {"models": {"deepseek-v4": {"cost": {"input": 1, "output": 2}}}}});
        let mut free = model("deepseek-v4");
        free.theos_per_request = Some("0".into());
        assert_eq!(
            entry(&free, "theos", Some(&prices), &[], None).billing,
            "free"
        );
        let mut paid = model("deepseek-v4");
        paid.theos_per_request = Some("2.5".into());
        let paid = entry(&paid, "theos", Some(&prices), &[], None);
        assert_eq!((paid.billing, paid.price.is_none()), ("theos", true));
        let keyed = entry(&model("deepseek-v4"), "api", Some(&prices), &[], None);
        assert_eq!(keyed.price.expect("price").input, 1.0);
    }

    /// Fetches the real catalogs once; run by hand (it uses the network).
    #[tokio::test]
    #[ignore = "uses the network"]
    async fn the_real_catalogs_answer() {
        let dir = tempfile::tempdir().expect("dir");
        let client = reqwest::Client::new();
        let (prices, stale) =
            cached(&client, dir.path(), "models-dev", PRICES_URL, PRICES_TTL).await;
        assert!(!stale);
        let prices = prices.expect("prices");
        println!("haiku: {:?}", price(&prices, "claude-haiku-4-5"));
        let url = format!("{SCORES_URL}?metric=aa_intelligence_index");
        let (rows, _) = cached(&client, dir.path(), "scores", &url, SCORES_TTL).await;
        let rows = rows
            .and_then(|rows| rows.as_array().cloned())
            .expect("rows");
        let first = rows[0]["modelId"].as_str().expect("id").to_string();
        let boards = vec![("aa_intelligence_index".to_string(), rows)];
        println!("{first}: {:?}", scores(&boards, &first));
        assert_eq!(scores(&boards, &first).len(), 1);
        // A second call reads the copy.
        let (again, stale) = cached(
            &client,
            dir.path(),
            "models-dev",
            "http://127.0.0.1:9/",
            PRICES_TTL,
        )
        .await;
        assert!(again.is_some() && !stale);
    }

    #[test]
    fn ids_are_normalized() {
        assert_eq!(normalized("openai/GPT-5.2"), "gpt-5-2");
        assert_eq!(
            without_date("claude-haiku-5-5-20261001"),
            "claude-haiku-5-5"
        );
        assert_eq!(without_date("gpt-5-2"), "gpt-5-2");
        assert_eq!(providers_of("glm-5.3-flash"), &["zai", "z-ai", "zhipuai"]);
        assert!(providers_of("someone-else").is_empty());
    }
}
