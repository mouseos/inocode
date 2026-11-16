//! Rule import with the model splitting sections, against a local stand-in
//! for a Chat Completions provider.

mod support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use inocode_core::store::Store;
use inocode_core::{Llm, LlmConfig, ProviderKind};
use serde_json::json;
use support::*;

const DOC: &str = "# Rules\n\n## Scope\n- Never push without approval.\n  - Ask first, every time.\n\n## Style\n- Write in English.\n";

fn llm(mock: &MockApi) -> Llm {
    Llm::new(LlmConfig {
        provider: ProviderKind::OpenAi,
        auth: inocode_core::oauth::AuthKind::ApiKey,
        base_url: mock.base_url.clone(),
        model: "splitter".to_string(),
        api_key: "test-key".to_string(),
        auth_store_path: None,
        reasoning: Default::default(),
    })
    .expect("client")
}

fn database() -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    std::env::temp_dir().join(format!("inocode-rules-{}-{nanos}.db", std::process::id()))
}

/// Answers each section with a split; `Style` gets a paraphrase while
/// `paraphrase_style` is set.
fn splitter(
    calls: Arc<AtomicUsize>,
    paraphrase_style: Arc<std::sync::atomic::AtomicBool>,
) -> impl Fn(&MockRequest) -> MockReply + Send + Sync + 'static {
    move |request: &MockRequest| {
        calls.fetch_add(1, Ordering::SeqCst);
        let prompt = request.body["messages"][1]["content"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let answer = if prompt.contains("Section: Rules > Scope") {
            json!({"rules": [{
                "quotes": ["Never push without approval.", "Ask first, every time."],
                "title": "No unapproved push",
                "condition": "before pushing",
                "keywords": "git push approval"
            }]})
        } else if paraphrase_style.load(Ordering::SeqCst) {
            json!({"rules": [{"quotes": ["Use English."]}]})
        } else {
            json!({"rules": [{"quotes": ["Write in English."], "keywords": "language"}]})
        };
        MockReply::stream(vec![sse_delta(&answer.to_string()), sse_done()])
    }
}

#[tokio::test]
async fn the_model_splits_by_quotation_and_unchanged_sections_are_not_resent() {
    let calls = Arc::new(AtomicUsize::new(0));
    let paraphrase = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let mock = start_mock(splitter(Arc::clone(&calls), Arc::clone(&paraphrase))).await;
    let llm = llm(&mock);
    let path = database();
    let store = Store::open(&path).await.expect("store");
    let project = store.project("p").await.expect("project");

    let first = store
        .import_rules_with(&llm, Some(project), "AGENTS.md", DOC)
        .await
        .expect("import");
    assert_eq!(calls.load(Ordering::SeqCst), 2, "one request per section");
    assert_eq!(first.added, 2, "one model rule and one mechanical rule");
    assert_eq!(first.fallbacks.len(), 1);
    assert_eq!(first.fallbacks[0].0, "Rules > Style");
    assert!(first.fallbacks[0].1.contains("not in the source"));

    let found = store
        .rules_for(
            Some(project),
            "push approval",
            5,
            std::time::Duration::from_secs(5),
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
        rule.body,
        "Never push without approval.\nAsk first, every time."
    );

    paraphrase.store(false, Ordering::SeqCst);
    let second = store
        .import_rules_with(&llm, Some(project), "AGENTS.md", DOC)
        .await
        .expect("import");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "the split section is not sent again; the fallen-back one is retried"
    );
    // Scope is untouched; Style moves from the mechanical block (with its
    // list marker) to the model's quote, so the old text is retired.
    assert_eq!((second.unchanged, second.added, second.retired), (1, 1, 1));
    assert!(second.fallbacks.is_empty());

    let edited = DOC.replace("## Style\n- Write in English.\n", "");
    let third = store
        .import_rules_with(&llm, Some(project), "AGENTS.md", &edited)
        .await
        .expect("import");
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    assert_eq!(
        (third.unchanged, third.retired),
        (1, 1),
        "a removed section is retired"
    );

    drop(store);
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
}
