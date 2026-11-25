//! Compares how rules reach the model, by whether it then picks the right
//! action (`fixtures/rules_behavior.json`):
//! - full: the whole rules document in the system prompt, as Codex does with
//!   AGENTS.md;
//! - rag: the lines the scenario's query retrieves (top 5, one line on each
//!   side added), as written;
//! - none: no rules, as a baseline.
//!
//! Ignored because it calls a real model; run by hand with the provider in
//! the environment. `INOCODE_EXPERIMENT_MODELS` (comma-separated) runs
//! several models of that provider.

mod support;

use std::collections::BTreeSet;
use std::time::Duration;

use inocode_core::conversation::{Body, Conversation, Message, Speaker};
use inocode_core::store::{ProjectId, Store};
use inocode_core::{JsonOutput, Llm, LlmConfig};
use serde_json::{json, Value};

const CONDITIONS: [&str; 3] = ["full", "rag", "none"];

fn database() -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "inocode-behavior-{}-{nanos}.db",
        std::process::id()
    ))
}

/// The retrieved lines with one neighbour on each side, in document order;
/// gaps are marked.
async fn retrieved(store: &Store, project: ProjectId, lines: &[&str], query: &str) -> String {
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
                picked.extend(index.saturating_sub(1)..=(index + 1).min(lines.len() - 1));
            }
        }
    }
    let mut out = String::new();
    let mut last = None;
    for index in picked {
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

async fn decide(llm: &Llm, system: String, scenario: &Value) -> Result<String, String> {
    let options = scenario["options"].as_object().expect("options");
    let letters: Vec<&String> = options.keys().collect();
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
    prompt.push_str("Choose the one action the agent should take. Answer with JSON only.");
    let conversation = Conversation {
        system: Some(system),
        messages: vec![Message {
            id: "task".to_string(),
            body: Body::User {
                from: Speaker::Person,
                parts: vec![prompt],
            },
        }],
    };
    let output = JsonOutput {
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
    };
    let mut stream = llm
        .converse_json(&conversation, &output)
        .await
        .map_err(|error| error.message)?;
    let mut answer = String::new();
    while let Some(delta) = stream.next().await.map_err(|error| error.message)? {
        answer.push_str(&delta);
    }
    let json = answer
        .find('{')
        .zip(answer.rfind('}'))
        .map(|(start, end)| &answer[start..=end])
        .ok_or_else(|| format!("no JSON in {answer:?}"))?;
    let value: Value = serde_json::from_str(json).map_err(|error| error.to_string())?;
    value["choice"]
        .as_str()
        .map(|choice| choice.trim().to_string())
        .ok_or_else(|| format!("no choice in {json}"))
}

#[tokio::test]
#[ignore = "calls a real model; run by hand"]
async fn whole_document_versus_retrieved_lines() {
    let fixtures: Value =
        serde_json::from_str(include_str!("fixtures/rules_behavior.json")).expect("fixtures");
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
        let mut totals = [(0usize, 0usize, 0usize); 3];
        for entry in fixtures["documents"].as_array().expect("documents") {
            let name = entry["name"].as_str().expect("name");
            let document = match name {
                "agents_complex_ja.md" => include_str!("fixtures/agents_complex_ja.md"),
                "agents_messy_ja.md" => include_str!("fixtures/agents_messy_ja.md"),
                other => panic!("unknown document {other}"),
            };
            let lines: Vec<&str> = document.lines().collect();
            let project = store
                .project(&format!("{model}:{name}"))
                .await
                .expect("project");
            for line in lines.iter().filter(|line| !line.trim().is_empty()) {
                store.add_rule(Some(project), line).await.expect("rule");
            }
            println!("== {name}");
            for scenario in entry["scenarios"].as_array().expect("scenarios") {
                let answer = scenario["answer"].as_str().expect("answer");
                let query = scenario["query"].as_str().expect("query");
                let mut row = Vec::new();
                for (slot, condition) in CONDITIONS.iter().enumerate() {
                    let rules = match *condition {
                        "full" => document.to_string(),
                        "rag" => retrieved(&store, project, &lines, query).await,
                        _ => String::new(),
                    };
                    let system = if rules.is_empty() {
                        "You are a coding agent working on a user's project.".to_string()
                    } else {
                        format!(
                            "You are a coding agent working on a user's project. Follow the project's rules below.\n----- RULES -----\n{rules}----- END RULES -----"
                        )
                    };
                    let chars = system.chars().count();
                    let outcome = decide(&llm, system, scenario).await;
                    let correct = outcome.as_deref() == Ok(answer);
                    totals[slot].0 += usize::from(correct);
                    totals[slot].1 += 1;
                    totals[slot].2 += chars;
                    row.push(match &outcome {
                        Ok(choice) if correct => format!("{condition}:{choice}○"),
                        Ok(choice) => format!("{condition}:{choice}×"),
                        Err(error) => format!(
                            "{condition}:error({})",
                            error.chars().take(40).collect::<String>()
                        ),
                    });
                }
                println!(
                    "  [{answer}] {} | {}",
                    row.join(" "),
                    scenario["situation"]
                        .as_str()
                        .unwrap_or_default()
                        .chars()
                        .take(30)
                        .collect::<String>()
                );
            }
        }
        for (slot, condition) in CONDITIONS.iter().enumerate() {
            let (correct, total, chars) = totals[slot];
            println!(
                "  {condition}: {correct}/{total} correct ({:.0}%), {:.0} system characters per call",
                100.0 * correct as f64 / total as f64,
                chars as f64 / total as f64
            );
        }
    }
    drop(store);
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
}
