//! Rule import with the model splitting sections by unit number, against a
//! local stand-in for a Chat Completions provider.

mod support;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use inocode_core::store::Store;
use inocode_core::{Llm, LlmConfig, ProviderKind};
use serde_json::json;
use support::*;

const DOC: &str = "# Rules\n\n## Scope\n- Never push without approval.\n  - Ask first, every time.\n\n## Style\n- Write in English.\n";

fn llm(mock: &MockApi) -> Arc<Llm> {
    Arc::new(
        Llm::new(LlmConfig {
            provider: ProviderKind::OpenAi,
            auth: inocode_core::oauth::AuthKind::ApiKey,
            base_url: mock.base_url.clone(),
            model: "splitter".to_string(),
            api_key: "test-key".to_string(),
            auth_store_path: None,
            reasoning: Default::default(),
        })
        .expect("client"),
    )
}

fn database() -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    std::env::temp_dir().join(format!("inocode-rules-{}-{nanos}.db", std::process::id()))
}

fn remove(path: &std::path::Path) {
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
}

/// Answers each section with a split; `Style` gets a unit number that does
/// not exist while `wrong_style` is set.
fn splitter(
    calls: Arc<AtomicUsize>,
    wrong_style: Arc<AtomicBool>,
) -> impl Fn(&MockRequest) -> MockReply + Send + Sync + 'static {
    move |request: &MockRequest| {
        calls.fetch_add(1, Ordering::SeqCst);
        let prompt = request.body["messages"][1]["content"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let answer = if prompt.contains("Heading (context only, not a unit): Rules > Scope") {
            assert!(prompt.contains("[1] - Never push without approval.\n[2] - Ask first"));
            json!({"rules": [{
                "units": [1, 2],
                "title": "No unapproved push",
                "condition": "before pushing",
                "keywords": "git push approval"
            }]})
        } else if wrong_style.load(Ordering::SeqCst) {
            json!({"rules": [{"units": [9]}]})
        } else {
            json!({"rules": [{"units": [1], "keywords": "language"}]})
        };
        MockReply::stream(vec![sse_delta(&answer.to_string()), sse_done()])
    }
}

#[tokio::test]
async fn the_model_splits_by_unit_and_unchanged_sections_are_not_resent() {
    let calls = Arc::new(AtomicUsize::new(0));
    let wrong = Arc::new(AtomicBool::new(true));
    let mock = start_mock(splitter(Arc::clone(&calls), Arc::clone(&wrong))).await;
    let llm = llm(&mock);
    let path = database();
    let store = Store::open(&path).await.expect("store");
    let project = store.project("p").await.expect("project");
    let import = |text: String| {
        let (store, llm) = (store.clone(), Arc::clone(&llm));
        async move {
            store
                .import_rules_with(llm, Some(project), "AGENTS.md", &text)
                .await
                .expect("import")
        }
    };

    let first = import(DOC.to_string()).await;
    assert_eq!(calls.load(Ordering::SeqCst), 2, "one request per section");
    assert_eq!(first.added, 2, "one model rule and one mechanical rule");
    assert_eq!(first.fallbacks.len(), 1);
    assert_eq!(first.fallbacks[0].0, "Rules > Style");
    assert!(first.fallbacks[0].1.contains("does not exist"));

    let found = store
        .rules_for(
            Some(project),
            "push approval",
            5,
            Duration::from_secs(5),
            None,
        )
        .await
        .expect("rules");
    assert_eq!(found.items.len(), 1);
    assert_eq!(found.items[0].title, "No unapproved push");
    let rule = store
        .note(found.items[0].id)
        .await
        .expect("note")
        .expect("exists");
    assert_eq!(
        rule.body, "- Never push without approval.\n\n- Ask first, every time.",
        "a rule is a slice of the source"
    );

    wrong.store(false, Ordering::SeqCst);
    let second = import(DOC.to_string()).await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "the split section is not sent again; the fallen-back one is retried"
    );
    // Rules are slices of the source, so the model's split of Style matches
    // the mechanical block it replaces.
    assert_eq!((second.unchanged, second.added, second.retired), (2, 0, 0));
    assert!(second.fallbacks.is_empty());

    let third = import(DOC.replace("## Style\n- Write in English.\n", "")).await;
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    assert_eq!(
        (third.unchanged, third.retired),
        (1, 1),
        "a removed section is retired"
    );

    drop(store);
    remove(&path);
}

/// Imports this repository's AGENTS.md through the provider configured in
/// the environment and prints what the model made of it. Ignored because it
/// needs network access and a credential; run it by hand.
#[tokio::test]
#[ignore = "requires a provider credential; run by hand"]
async fn live_split_of_this_repository_agents_md() {
    let markdown = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../AGENTS.md"))
        .expect("AGENTS.md");
    let llm = Arc::new(Llm::from_env().expect("provider"));
    let path = database();
    let store = Store::open(&path).await.expect("store");
    let project = store.project("inocode").await.expect("project");
    let started = std::time::Instant::now();
    let report = store
        .import_rules_with(llm, Some(project), "AGENTS.md", &markdown)
        .await
        .expect("import");
    println!("elapsed: {:?}", started.elapsed());
    println!("rules added: {}", report.added);
    println!("sections split mechanically: {}", report.fallbacks.len());
    for (section, reason) in &report.fallbacks {
        println!("  fallback [{section}]: {reason}");
    }
    println!("unassigned fragments: {}", report.unassigned.len());
    for fragment in &report.unassigned {
        let shown: String = fragment.chars().take(80).collect();
        println!("  unassigned: {shown:?}");
    }
    for query in ["コミット", "研究根拠", "変更履歴"] {
        let found = store
            .rules_for(Some(project), query, 3, Duration::from_secs(5), None)
            .await
            .expect("rules");
        for hit in found.items {
            let note = store.note(hit.id).await.expect("note").expect("exists");
            let first: String = note.body.chars().take(60).collect();
            println!(
                "  [{query}] {} | when: {} | {first:?}",
                note.title, note.condition
            );
        }
    }
    drop(store);
    remove(&path);
}
