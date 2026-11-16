//! Compares two ways of making rules retrievable, on fixed scenarios with
//! gold lines (`fixtures/rules_scenarios.json`):
//! - A, paragraphs: the document cut at blank lines, stored as written, no
//!   model involved;
//! - A2, lines: every non-blank line as its own entry, no model involved;
//! - B, extraction: the model extracts rules (statements plus located source
//!   passages) from the document.
//!
//! Both are searched the same way (FTS5 trigram, BM25). A scenario is a hit
//! at k when one of the top k results covers a gold line. Ignored because B
//! needs a provider credential; run by hand with the provider in the
//! environment; `INOCODE_EXPERIMENT_SKIP_EXTRACTION=1` runs A and A2 only.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use inocode_core::store::{ProjectId, Store};
use inocode_core::Llm;
use serde_json::Value;

const KS: [usize; 3] = [1, 3, 5];

fn database() -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "inocode-experiment-{}-{nanos}.db",
        std::process::id()
    ))
}

/// Blocks of consecutive non-blank lines.
fn paragraphs(document: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    for line in document.lines() {
        if line.trim().is_empty() {
            if !current.is_empty() {
                out.push(current.join("\n"));
                current.clear();
            }
        } else {
            current.push(line);
        }
    }
    if !current.is_empty() {
        out.push(current.join("\n"));
    }
    out
}

/// Pieces starting at each `## ` heading (what precedes the first is its
/// own piece), so a long document fits the model's output.
fn sections(document: &str) -> Vec<String> {
    let mut out: Vec<String> = vec![String::new()];
    for line in document.lines() {
        if line.starts_with("## ") {
            out.push(String::new());
        }
        let last = out.last_mut().expect("one piece");
        last.push_str(line);
        last.push('\n');
    }
    out.retain(|piece| !piece.trim().is_empty());
    out
}

/// The 1-based document lines a stored rule's text comes from.
fn lines_of(document_lines: &[&str], body: &str) -> HashSet<usize> {
    let mut lines = HashSet::new();
    for part in body.lines().map(str::trim).filter(|part| !part.is_empty()) {
        for (index, line) in document_lines.iter().enumerate() {
            if line.contains(part) {
                lines.insert(index + 1);
            }
        }
    }
    lines
}

/// Whether a query has a term the trigram index can search (three or more
/// characters).
fn searchable(query: &str) -> bool {
    query
        .split_whitespace()
        .any(|term| term.chars().count() >= 3)
}

struct Score {
    hits: [usize; 3],
    injected_chars: usize,
    unsearchable: usize,
    total: usize,
}

async fn score(
    store: &Store,
    project: ProjectId,
    document: &str,
    scenarios: &[Value],
    with_statements: bool,
    show_misses: bool,
) -> Score {
    let document_lines: Vec<&str> = document.lines().collect();
    let mut score = Score {
        hits: [0; 3],
        injected_chars: 0,
        unsearchable: 0,
        total: scenarios.len(),
    };
    for scenario in scenarios {
        let query = scenario["query"].as_str().expect("query");
        let gold: HashSet<usize> = scenario["gold"]
            .as_array()
            .expect("gold")
            .iter()
            .map(|line| line.as_u64().expect("line") as usize)
            .collect();
        if !searchable(query) {
            score.unsearchable += 1;
        }
        let found = store
            .rules_for(Some(project), query, 5, Duration::from_secs(5), None)
            .await
            .expect("search")
            .items;
        let mut first_hit = None;
        for (rank, hit) in found.iter().enumerate() {
            let note = store.note(hit.id).await.expect("note").expect("exists");
            if rank < 3 {
                score.injected_chars += note.body.chars().count();
                if with_statements {
                    score.injected_chars += note.summary.chars().count();
                }
            }
            if first_hit.is_none() && !lines_of(&document_lines, &note.body).is_disjoint(&gold) {
                first_hit = Some(rank);
            }
        }
        for (slot, k) in KS.iter().enumerate() {
            if first_hit.is_some_and(|rank| rank < *k) {
                score.hits[slot] += 1;
            }
        }
        if show_misses && first_hit.is_none_or(|rank| rank >= 3) {
            println!(
                "    miss@3: {} [{}]{}",
                scenario["situation"].as_str().unwrap_or_default(),
                query,
                if searchable(query) {
                    ""
                } else {
                    " (no searchable term)"
                }
            );
        }
    }
    score
}

fn report(label: &str, score: &Score, imported: usize, import_time: Duration) {
    let rate = |hits: usize| 100.0 * hits as f64 / score.total as f64;
    println!(
        "  {label}: recall@1 {:.0}% | @3 {:.0}% | @5 {:.0}% | chars in top 3 per query {:.0} | entries {imported} | import {:.1}s | queries without a searchable term {}",
        rate(score.hits[0]),
        rate(score.hits[1]),
        rate(score.hits[2]),
        score.injected_chars as f64 / score.total as f64,
        import_time.as_secs_f64(),
        score.unsearchable
    );
}

#[tokio::test]
#[ignore = "requires a provider credential; run by hand"]
async fn paragraphs_versus_extraction() {
    let scenarios: Value =
        serde_json::from_str(include_str!("fixtures/rules_scenarios.json")).expect("scenarios");
    let llm = Arc::new(Llm::from_env().expect("provider"));
    let path = database();
    let store = Store::open(&path).await.expect("store");
    for entry in scenarios["documents"].as_array().expect("documents") {
        let name = entry["name"].as_str().expect("name");
        let document = match name {
            "AGENTS.md" => include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../AGENTS.md")),
            "agents_messy_ja.md" => include_str!("fixtures/agents_messy_ja.md"),
            other => panic!("unknown document {other}"),
        };
        let cases = entry["scenarios"].as_array().expect("scenarios");
        println!("== {name} ({} scenarios)", cases.len());

        let a = store.project(&format!("A:{name}")).await.expect("project");
        let started = Instant::now();
        let pieces = paragraphs(document);
        for piece in &pieces {
            store.add_rule(Some(a), piece).await.expect("rule");
        }
        let a_time = started.elapsed();
        let a_score = score(&store, a, document, cases, false, true).await;
        report("A paragraphs", &a_score, pieces.len(), a_time);

        let a2 = store.project(&format!("A2:{name}")).await.expect("project");
        let started = Instant::now();
        let lines: Vec<&str> = document
            .lines()
            .filter(|line| !line.trim().is_empty())
            .collect();
        for line in &lines {
            store.add_rule(Some(a2), line).await.expect("rule");
        }
        let a2_time = started.elapsed();
        let a2_score = score(&store, a2, document, cases, false, true).await;
        report("A2 lines", &a2_score, lines.len(), a2_time);

        if std::env::var("INOCODE_EXPERIMENT_SKIP_EXTRACTION").is_ok() {
            continue;
        }
        let b = store.project(&format!("B:{name}")).await.expect("project");
        let started = Instant::now();
        let parts = if name == "AGENTS.md" {
            sections(document)
        } else {
            vec![document.to_string()]
        };
        let mut added = 0;
        for (index, part) in parts.iter().enumerate() {
            let report = store
                .import_rules(
                    Some(Arc::clone(&llm)),
                    Some(b),
                    &format!("{name}#{index}"),
                    part,
                )
                .await
                .expect("import");
            added += report.added;
            if let Some(error) = report.extraction_error {
                println!("    extraction of part {index} fell back to the whole part: {error}");
            }
            if report.unverified_sources > 0 {
                println!(
                    "    part {index}: {} sources not found in the text",
                    report.unverified_sources
                );
            }
        }
        let b_time = started.elapsed();
        let b_score = score(&store, b, document, cases, true, true).await;
        report("B extraction", &b_score, added, b_time);
    }
    drop(store);
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
}
