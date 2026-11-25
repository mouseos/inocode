//! Looks for ways to give rules to the model with fewer tokens than the
//! whole document (Codex's AGENTS.md) at the same decision accuracy, on the
//! scenarios of `fixtures/rules_behavior.json`:
//! - full: the whole document in the system prompt;
//! - rag: the lines the scenario's query retrieves (top 5) and one line on
//!   each side;
//! - rag_links: rag, plus lines sharing a rare three-character sequence with
//!   a retrieved line (so a rule and its later definition or exception come
//!   together; no language-specific processing);
//! - index: a short index (each line's first 20 characters, numbered) and a
//!   `read_rules` tool that returns chosen lines in full;
//! - rag_self, rag_links_self: rag and rag_links searched with keywords the
//!   model writes from the situation (that call's tokens count too).
//!
//! `INOCODE_EXPERIMENT_CONDITIONS` (comma-separated) runs a subset.
//!
//! Documents: the complex and the messy fixtures, and a long one made of
//! this repository's AGENTS.md followed by both. Tokens are the provider's
//! reported usage, summed over the calls of a decision. Ignored because it
//! calls a real model; `INOCODE_EXPERIMENT_MODELS` (comma-separated) runs
//! several models of the configured provider.

mod support;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::time::Duration;

use inocode_core::conversation::{AssistantPart, Body, Conversation, Message, Speaker, ToolSpec};
use inocode_core::store::{ProjectId, Store};
use inocode_core::{ChatStream, JsonOutput, Llm, LlmConfig, TokenUsage};
use serde_json::{json, Value};

const CONDITIONS: [&str; 6] = [
    "full",
    "rag",
    "rag_links",
    "rag_self",
    "rag_links_self",
    "index",
];
const INDEX_CHARS: usize = 20;
const RARE_MAX_LINES: usize = 3;
const LINKS_PER_LINE: usize = 5;

fn database() -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "inocode-efficiency-{}-{nanos}.db",
        std::process::id()
    ))
}

/// Lines (0-based) joined to other lines by a three-character sequence that
/// at most `RARE_MAX_LINES` lines contain.
fn links(lines: &[&str]) -> HashMap<usize, Vec<usize>> {
    let grams: Vec<HashSet<String>> = lines
        .iter()
        .map(|line| {
            let chars: Vec<char> = line.chars().collect();
            chars
                .windows(3)
                .filter(|window| window.iter().all(|c| c.is_alphanumeric()))
                .map(|window| window.iter().collect())
                .collect()
        })
        .collect();
    let mut holders: HashMap<&str, Vec<usize>> = HashMap::new();
    for (index, set) in grams.iter().enumerate() {
        for gram in set {
            holders.entry(gram.as_str()).or_default().push(index);
        }
    }
    let mut out: HashMap<usize, Vec<usize>> = HashMap::new();
    for owners in holders.values() {
        if (2..=RARE_MAX_LINES).contains(&owners.len()) {
            for &a in owners {
                for &b in owners {
                    if a != b {
                        let list = out.entry(a).or_default();
                        if !list.contains(&b) && list.len() < LINKS_PER_LINE {
                            list.push(b);
                        }
                    }
                }
            }
        }
    }
    out
}

fn render(lines: &[&str], picked: &BTreeSet<usize>) -> String {
    let mut out = String::new();
    let mut last = None;
    for &index in picked {
        if lines[index].trim().is_empty() {
            continue;
        }
        if last.is_some_and(|last| index > last + 1) {
            out.push_str("…\n");
        }
        out.push_str(lines[index]);
        out.push('\n');
        last = Some(index);
    }
    out
}

fn with_neighbours(picked: &mut BTreeSet<usize>, index: usize, count: usize) {
    picked.extend(index.saturating_sub(1)..=(index + 1).min(count - 1));
}

async fn retrieve(
    store: &Store,
    project: ProjectId,
    lines: &[&str],
    query: &str,
    linked: Option<&HashMap<usize, Vec<usize>>>,
) -> String {
    let found = store
        .rules_for(Some(project), query, 5, Duration::from_secs(5), None)
        .await
        .expect("search")
        .items;
    let mut picked = BTreeSet::new();
    for hit in found {
        let note = store.note(hit.id).await.expect("note").expect("exists");
        for (index, line) in lines.iter().enumerate() {
            if line.trim() == note.body.trim() {
                with_neighbours(&mut picked, index, lines.len());
                for &other in linked
                    .and_then(|links| links.get(&index))
                    .into_iter()
                    .flatten()
                {
                    with_neighbours(&mut picked, other, lines.len());
                }
            }
        }
    }
    render(lines, &picked)
}

fn index_of(lines: &[&str]) -> String {
    let mut out = String::new();
    for (index, line) in lines.iter().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let head: String = line.chars().take(INDEX_CHARS).collect();
        let more = if line.chars().count() > INDEX_CHARS {
            "…"
        } else {
            ""
        };
        out.push_str(&format!("{}: {head}{more}\n", index + 1));
    }
    out
}

fn options_prompt(scenario: &Value) -> (String, Vec<String>) {
    let options = scenario["options"].as_object().expect("options");
    let mut prompt = format!(
        "Situation: {}\nOptions:\n",
        scenario["situation"].as_str().expect("situation")
    );
    for (letter, text) in options {
        prompt.push_str(&format!(
            "{letter}. {}\n",
            text.as_str().unwrap_or_default()
        ));
    }
    (prompt, options.keys().cloned().collect())
}

fn choice_of(answer: &str) -> Result<String, String> {
    let json = answer
        .find('{')
        .zip(answer.rfind('}'))
        .filter(|(start, end)| start < end)
        .map(|(start, end)| &answer[start..=end])
        .ok_or_else(|| {
            format!(
                "no JSON in {:?}",
                answer.chars().take(60).collect::<String>()
            )
        })?;
    let value: Value = serde_json::from_str(json).map_err(|error| error.to_string())?;
    value["choice"]
        .as_str()
        .map(|choice| choice.trim().to_string())
        .ok_or_else(|| "no choice".to_string())
}

#[derive(Default)]
struct Spent {
    input: u64,
    output: u64,
    cached: u64,
    calls: u64,
    unreported: u64,
}

impl Spent {
    fn add(&mut self, usage: Option<TokenUsage>) {
        self.calls += 1;
        match usage {
            Some(usage) => {
                self.input += usage.input;
                self.output += usage.output;
                self.cached += usage.cached;
            }
            None => self.unreported += 1,
        }
    }
}

async fn drain(mut stream: ChatStream) -> Result<(String, ChatStream), String> {
    let mut text = String::new();
    while let Some(delta) = stream.next().await.map_err(|error| error.message)? {
        text.push_str(&delta);
    }
    Ok((text, stream))
}

fn decision_output(letters: &[String]) -> JsonOutput {
    JsonOutput {
        name: "decision".to_string(),
        schema: json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["choice", "reason"],
            "properties": {
                "choice": {"type": "string", "enum": letters},
                "reason": {"type": "string"}
            }
        }),
    }
}

fn user(text: String) -> Message {
    Message {
        id: "task".to_string(),
        body: Body::User {
            from: Speaker::Person,
            parts: vec![text],
        },
    }
}

const AGENT: &str = "You are a coding agent working on a user's project.";

/// One decision with the rules given in `system`.
async fn decide(
    llm: &Llm,
    system: String,
    scenario: &Value,
    spent: &mut Spent,
) -> Result<String, String> {
    let (mut prompt, letters) = options_prompt(scenario);
    prompt.push_str("Choose the one action the agent should take. Answer with JSON only.");
    let conversation = Conversation {
        system: Some(system),
        messages: vec![user(prompt)],
    };
    let stream = llm
        .converse_json(&conversation, &decision_output(&letters))
        .await
        .map_err(|error| error.message)?;
    let (text, stream) = drain(stream).await?;
    spent.add(stream.usage());
    choice_of(&text)
}

/// One decision with only an index in context and a tool to read lines.
async fn decide_with_index(
    llm: &Llm,
    lines: &[&str],
    scenario: &Value,
    spent: &mut Spent,
    reads: &mut u64,
) -> Result<String, String> {
    let system = format!(
        "{AGENT} The project has rules. Below is an index: each rule line's number and its first words. Before deciding, call read_rules with the numbers of every line that may matter, then follow the rules you read.\n----- RULE INDEX -----\n{}----- END INDEX -----",
        index_of(lines)
    );
    let tool = ToolSpec {
        name: "read_rules".to_string(),
        description: "Returns the full text of the given rule lines, with one line on each side."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {"lines": {"type": "array", "items": {"type": "integer"}}},
            "required": ["lines"]
        }),
    };
    let (mut prompt, letters) = options_prompt(scenario);
    prompt.push_str("Choose the one action the agent should take.");
    let mut conversation = Conversation {
        system: Some(system),
        messages: vec![user(prompt)],
    };
    let stream = llm
        .converse(&conversation, &[tool])
        .await
        .map_err(|error| error.message)?;
    let (text, mut stream) = drain(stream).await?;
    spent.add(stream.usage());
    let turn = stream.assistant();
    let calls: Vec<(String, String)> = match &turn {
        Body::Assistant { parts, .. } => parts
            .iter()
            .filter_map(|part| match part {
                AssistantPart::ToolCall {
                    call_id, arguments, ..
                } => Some((call_id.clone(), arguments.clone())),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    if calls.is_empty() {
        // Answered without reading; take its choice if it gave one.
        return choice_of(&text);
    }
    conversation.messages.push(Message {
        id: "turn".to_string(),
        body: turn,
    });
    for (number, (call_id, arguments)) in calls.into_iter().enumerate() {
        *reads += 1;
        let wanted: Vec<usize> = serde_json::from_str::<Value>(&arguments)
            .ok()
            .and_then(|value| value["lines"].as_array().cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(Value::as_u64)
            .map(|line| line as usize)
            .filter(|&line| line >= 1 && line <= lines.len())
            .collect();
        let mut picked = BTreeSet::new();
        for line in wanted {
            with_neighbours(&mut picked, line - 1, lines.len());
        }
        conversation.messages.push(Message {
            id: format!("result-{number}"),
            body: Body::ToolResult {
                call_id,
                name: "read_rules".to_string(),
                content: render(lines, &picked),
                is_error: false,
                images: Vec::new(),
            },
        });
    }
    conversation.messages.push(user(
        "Now choose the one action the agent should take. Answer with JSON only.".to_string(),
    ));
    let stream = llm
        .converse_json(&conversation, &decision_output(&letters))
        .await
        .map_err(|error| error.message)?;
    let (text, stream) = drain(stream).await?;
    spent.add(stream.usage());
    choice_of(&text)
}

/// Search keywords the model writes for a situation, knowing nothing of
/// the rules document's wording.
async fn keywords(llm: &Llm, scenario: &Value) -> Result<(String, Option<TokenUsage>), String> {
    let situation = scenario["situation"].as_str().expect("situation");
    let conversation = Conversation {
        system: Some(format!(
            "{AGENT} Before acting you search the project's rules document with a full-text index."
        )),
        messages: vec![user(format!(
            "Situation: {situation}
Write 3 to 6 search keywords, separated by spaces, that would find the project rules relevant to this situation. Use words likely to appear in a rules document, in the situation's language. Answer with JSON only."
        ))],
    };
    let output = JsonOutput {
        name: "keywords".to_string(),
        schema: json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["keywords"],
            "properties": {"keywords": {"type": "string"}}
        }),
    };
    let stream = llm
        .converse_json(&conversation, &output)
        .await
        .map_err(|error| error.message)?;
    let (text, stream) = drain(stream).await?;
    let json = text
        .find('{')
        .zip(text.rfind('}'))
        .map(|(start, end)| &text[start..=end])
        .ok_or("no JSON")?;
    let value: Value = serde_json::from_str(json).map_err(|error| error.to_string())?;
    let words = value["keywords"].as_str().unwrap_or_default().to_string();
    Ok((words, stream.usage()))
}

#[tokio::test]
#[ignore = "calls a real model; run by hand"]
async fn fewer_tokens_at_the_same_accuracy() {
    let fixtures: Value =
        serde_json::from_str(include_str!("fixtures/rules_behavior.json")).expect("fixtures");
    let complex = include_str!("fixtures/agents_complex_ja.md");
    let messy = include_str!("fixtures/agents_messy_ja.md");
    let agents =
        include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../AGENTS.md")).replace("\r\n", "\n");
    let long = format!("{agents}\n\n{complex}\n\n{messy}");
    let mut sets: Vec<(String, String, Vec<Value>)> = Vec::new();
    let mut all = Vec::new();
    for entry in fixtures["documents"].as_array().expect("documents") {
        let name = entry["name"].as_str().expect("name");
        let document = if name == "agents_complex_ja.md" {
            complex
        } else {
            messy
        };
        let scenarios = entry["scenarios"].as_array().expect("scenarios").clone();
        all.extend(scenarios.iter().cloned());
        sets.push((name.to_string(), document.to_string(), scenarios));
    }
    sets.push(("long (AGENTS.md + both)".to_string(), long, all));

    let base = support::live_config();
    let models: Vec<String> = std::env::var("INOCODE_EXPERIMENT_MODELS")
        .map(|list| {
            list.split(',')
                .map(|model| model.trim().to_string())
                .collect()
        })
        .unwrap_or_else(|_| vec![base.model.clone()]);
    let path = database();
    let store = Store::open(&path).await.expect("store");

    for model in models {
        let llm = Llm::new(LlmConfig {
            model: model.clone(),
            ..base.clone()
        })
        .expect("client");
        println!("#### model {model}");
        for (name, document, scenarios) in &sets {
            let lines: Vec<&str> = document.lines().collect();
            let linked = links(&lines);
            let project = store
                .project(&format!("{model}:{name}"))
                .await
                .expect("project");
            for line in lines.iter().filter(|line| !line.trim().is_empty()) {
                store.add_rule(Some(project), line).await.expect("rule");
            }
            println!(
                "== {name}: {} characters, {} scenarios",
                document.chars().count(),
                scenarios.len()
            );
            let chosen: Option<Vec<String>> = std::env::var("INOCODE_EXPERIMENT_CONDITIONS")
                .ok()
                .map(|list| {
                    list.split(',')
                        .map(|item| item.trim().to_string())
                        .collect()
                });
            let mut written: std::collections::HashMap<String, (String, Option<TokenUsage>)> =
                std::collections::HashMap::new();
            for condition in CONDITIONS {
                if chosen
                    .as_ref()
                    .is_some_and(|chosen| !chosen.iter().any(|item| item == condition))
                {
                    continue;
                }
                let mut spent = Spent::default();
                let mut correct = 0;
                let mut reads = 0;
                let mut wrong = Vec::new();
                for scenario in scenarios {
                    let answer = scenario["answer"].as_str().expect("answer");
                    let mut query = scenario["query"].as_str().expect("query").to_string();
                    if condition.ends_with("_self") {
                        let key = scenario["situation"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string();
                        if !written.contains_key(&key) {
                            let made = keywords(&llm, scenario).await.unwrap_or_else(|error| {
                                (String::new(), {
                                    println!("      keywords failed: {error}");
                                    None
                                })
                            });
                            written.insert(key.clone(), made);
                        }
                        let (words, usage) = written[&key].clone();
                        spent.add(usage);
                        query = words;
                    }
                    let query = query.as_str();
                    let outcome = match condition {
                        "index" => {
                            decide_with_index(&llm, &lines, scenario, &mut spent, &mut reads).await
                        }
                        _ => {
                            let rules = match condition {
                                "full" => document.clone(),
                                "rag" | "rag_self" => {
                                    retrieve(&store, project, &lines, query, None).await
                                }
                                _ => retrieve(&store, project, &lines, query, Some(&linked)).await,
                            };
                            let system = format!(
                                "{AGENT} Follow the project's rules below.\n----- RULES -----\n{rules}----- END RULES -----"
                            );
                            decide(&llm, system, scenario, &mut spent).await
                        }
                    };
                    if outcome.as_deref() == Ok(answer) {
                        correct += 1;
                    } else if condition.ends_with("_self") {
                        let situation: String = scenario["situation"]
                            .as_str()
                            .unwrap_or_default()
                            .chars()
                            .take(24)
                            .collect();
                        wrong.push(format!(
                            "{situation} [keywords: {query}]→{}",
                            outcome
                                .clone()
                                .unwrap_or_else(|error| format!("error {error}"))
                        ));
                    } else {
                        let situation: String = scenario["situation"]
                            .as_str()
                            .unwrap_or_default()
                            .chars()
                            .take(24)
                            .collect();
                        wrong.push(format!(
                            "{situation}→{}",
                            outcome.unwrap_or_else(|error| format!("error {error}"))
                        ));
                    }
                }
                let n = scenarios.len() as f64;
                println!(
                    "  {condition:9} {correct}/{} correct | input {:.0} (cached {:.0}) + output {:.0} tokens per decision | calls {} | unreported {}{}",
                    scenarios.len(),
                    spent.input as f64 / n,
                    spent.cached as f64 / n,
                    spent.output as f64 / n,
                    spent.calls,
                    spent.unreported,
                    if condition == "index" { format!(" | read_rules calls {reads}") } else { String::new() }
                );
                for item in wrong {
                    println!("      wrong: {item}");
                }
            }
        }
    }
    drop(store);
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
}

/// Where the whole document stops being cheaper than linked retrieval with
/// the model's own keywords: the same 18 scenarios over rule documents of
/// growing length, with the scenario rules in the middle of unrelated rules
/// (`fixtures/filler_rules_*`, which reuse words such as test, commit and
/// database). `INOCODE_EXPERIMENT_REPEATS` repeats each condition.
#[tokio::test]
#[ignore = "calls a real model; run by hand"]
async fn crossover_by_length() {
    let fixtures: Value =
        serde_json::from_str(include_str!("fixtures/rules_behavior.json")).expect("fixtures");
    let scenarios: Vec<Value> = fixtures["documents"]
        .as_array()
        .expect("documents")
        .iter()
        .flat_map(|entry| entry["scenarios"].as_array().expect("scenarios").clone())
        .collect();
    let target = format!(
        "{}\n\n{}",
        include_str!("fixtures/agents_complex_ja.md"),
        include_str!("fixtures/agents_messy_ja.md")
    );
    let a = include_str!("fixtures/filler_rules_a_ja.md");
    let b = include_str!("fixtures/filler_rules_b_ja.md");
    let agents =
        include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../AGENTS.md")).replace("\r\n", "\n");
    let guide = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../docs/evidence/README.md"
    ))
    .replace("\r\n", "\n");
    let documents = [
        target.clone(),
        format!("{a}\n\n{target}"),
        format!("{a}\n\n{target}\n\n{b}"),
        format!("{a}\n\n{agents}\n\n{target}\n\n{b}"),
        format!("{a}\n\n{agents}\n\n{guide}\n\n{target}\n\n{b}"),
    ];
    let repeats: usize = std::env::var("INOCODE_EXPERIMENT_REPEATS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(1);
    let base = support::live_config();
    let models: Vec<String> = std::env::var("INOCODE_EXPERIMENT_MODELS")
        .map(|list| {
            list.split(',')
                .map(|model| model.trim().to_string())
                .collect()
        })
        .unwrap_or_else(|_| vec![base.model.clone()]);
    let path = database();
    let store = Store::open(&path).await.expect("store");
    for model in models {
        let llm = Llm::new(LlmConfig {
            model: model.clone(),
            ..base.clone()
        })
        .expect("client");
        println!(
            "#### model {model}, {repeats} repeat(s), {} scenarios",
            scenarios.len()
        );
        for (number, document) in documents.iter().enumerate() {
            let lines: Vec<&str> = document.lines().collect();
            let linked = links(&lines);
            let project = store
                .project(&format!("{model}:length-{number}"))
                .await
                .expect("project");
            for line in lines.iter().filter(|line| !line.trim().is_empty()) {
                store.add_rule(Some(project), line).await.expect("rule");
            }
            let mut written: HashMap<String, (String, Option<TokenUsage>)> = HashMap::new();
            for condition in ["full", "rag_links_self"] {
                let mut spent = Spent::default();
                let mut correct = 0;
                let mut wrong = Vec::new();
                for repeat in 0..repeats {
                    for scenario in &scenarios {
                        let answer = scenario["answer"].as_str().expect("answer");
                        let rules = if condition == "full" {
                            document.clone()
                        } else {
                            let key = format!(
                                "{repeat}:{}",
                                scenario["situation"].as_str().unwrap_or_default()
                            );
                            if !written.contains_key(&key) {
                                let made = keywords(&llm, scenario).await.unwrap_or_default();
                                written.insert(key.clone(), made);
                            }
                            let (words, usage) = written[&key].clone();
                            spent.add(usage);
                            retrieve(&store, project, &lines, &words, Some(&linked)).await
                        };
                        let system = format!(
                            "{AGENT} Follow the project's rules below.\n----- RULES -----\n{rules}----- END RULES -----"
                        );
                        let outcome = decide(&llm, system, scenario, &mut spent).await;
                        if outcome.as_deref() == Ok(answer) {
                            correct += 1;
                        } else {
                            let situation: String = scenario["situation"]
                                .as_str()
                                .unwrap_or_default()
                                .chars()
                                .take(20)
                                .collect();
                            wrong.push(situation);
                        }
                    }
                }
                let decisions = (scenarios.len() * repeats) as f64;
                println!(
                    "  {} chars | {condition:14} {correct}/{} correct | input {:.0} (cached {:.0}) + output {:.0} tokens per decision | unreported {}",
                    document.chars().count(),
                    scenarios.len() * repeats,
                    spent.input as f64 / decisions,
                    spent.cached as f64 / decisions,
                    spent.output as f64 / decisions,
                    spent.unreported
                );
                if !wrong.is_empty() {
                    println!("      wrong: {}", wrong.join(" / "));
                }
            }
        }
    }
    drop(store);
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
}
