//! Rule import by whole-document extraction, against a local stand-in for a
//! Chat Completions provider, plus a hand-run check against a real model.

mod support;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use inocode_core::store::Store;
use inocode_core::{Llm, LlmConfig, ProviderKind};
use serde_json::json;
use support::*;

const DOC: &str = "# Rules\n\n- Never **push** without approval.\n- Write comments in English.\n";

fn llm(mock: &MockApi) -> Arc<Llm> {
    Arc::new(
        Llm::new(LlmConfig {
            provider: ProviderKind::OpenAi,
            auth: inocode_core::oauth::AuthKind::ApiKey,
            base_url: mock.base_url.clone(),
            model: "extractor".to_string(),
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

/// Extracts one rule per bullet of the request's document (quoting it with
/// the markup dropped, as models do), or answers nonsense while `broken`.
fn extractor(
    calls: Arc<AtomicUsize>,
    broken: Arc<AtomicBool>,
) -> impl Fn(&MockRequest) -> MockReply + Send + Sync + 'static {
    move |request: &MockRequest| {
        calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(
            request.body["response_format"]["json_schema"]["name"], "rule_extraction",
            "the answer shape is requested"
        );
        let prompt = request.body["messages"][1]["content"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let answer = if broken.load(Ordering::SeqCst) {
            "sorry".to_string()
        } else {
            let rules: Vec<_> = prompt
                .lines()
                .filter_map(|line| line.strip_prefix("- "))
                .map(|bullet| {
                    let quote = bullet.replace("**", "");
                    json!({
                        "statement": format!("The agent must: {quote}"),
                        "condition": null,
                        "exceptions": [],
                        "reason": null,
                        "strength": "must",
                        "sources": [quote],
                        "section": "Rules"
                    })
                })
                .collect();
            json!({ "rules": rules }).to_string()
        };
        MockReply::stream(vec![sse_delta(&answer), sse_done()])
    }
}

#[tokio::test]
async fn rules_are_extracted_from_the_whole_document() {
    let calls = Arc::new(AtomicUsize::new(0));
    let broken = Arc::new(AtomicBool::new(true));
    let mock = start_mock(extractor(Arc::clone(&calls), Arc::clone(&broken))).await;
    let llm = llm(&mock);
    let path = database();
    let store = Store::open(&path).await.expect("store");
    let project = store.project("p").await.expect("project");
    let import = |text: String| {
        let (store, llm) = (store.clone(), Arc::clone(&llm));
        async move {
            store
                .import_rules(Some(llm), Some(project), "AGENTS.md", &text)
                .await
                .expect("import")
        }
    };
    let search = |text: &'static str| {
        let store = store.clone();
        async move {
            store
                .rules_for(Some(project), text, 10, Duration::from_secs(5), None)
                .await
                .expect("rules")
                .items
        }
    };

    let first = import(DOC.to_string()).await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "one request for the document"
    );
    assert!(first.extraction_error.is_some());
    assert_eq!(
        first.added, 1,
        "a failed extraction keeps the whole document"
    );

    broken.store(false, Ordering::SeqCst);
    let second = import(DOC.to_string()).await;
    assert_eq!(calls.load(Ordering::SeqCst), 2, "the extraction is retried");
    assert_eq!((second.added, second.retired), (2, 1));
    assert_eq!(second.unverified_sources, 0);
    assert_eq!(second.uncovered, ["Rules"]);
    let found = search("approval").await;
    assert_eq!(found.len(), 1);
    let rule = store
        .note(found[0].id)
        .await
        .expect("note")
        .expect("exists");
    assert_eq!(
        rule.body, "Never **push** without approval.",
        "the stored rule is the source text, markup kept"
    );
    assert_eq!(
        search("agent must").await.len(),
        2,
        "the statements are searchable"
    );

    let third = import(DOC.to_string()).await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "an extracted document is not resent"
    );
    assert_eq!(third.unchanged, 2);

    let edited = format!("{DOC}- Run the tests before committing.\n");
    let fourth = import(edited).await;
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    assert_eq!((fourth.unchanged, fourth.added, fourth.retired), (2, 1, 0));

    drop(store);
    remove(&path);
}

/// Extracts this repository's AGENTS.md and a messy hand-written one through
/// the provider configured in the environment, and prints what came out.
/// Ignored because it needs network access and a credential; run it by hand.
#[tokio::test]
#[ignore = "requires a provider credential; run by hand"]
async fn live_extraction_of_the_fixture_documents() {
    let llm = Arc::new(support::live_llm());
    let documents = [
        (
            "AGENTS.md",
            include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../AGENTS.md")),
            "コミット 研究根拠 変更履歴",
        ),
        (
            "agents_messy_ja.md",
            include_str!("fixtures/agents_messy_ja.md"),
            "バックアップ 本番環境 土日",
        ),
    ];
    for (name, document, query) in documents {
        let path = database();
        let store = Store::open(&path).await.expect("store");
        let project = store.project(name).await.expect("project");
        let started = std::time::Instant::now();
        let report = store
            .import_rules(Some(Arc::clone(&llm)), Some(project), name, document)
            .await
            .expect("import");
        println!("== {name}: {:?}", started.elapsed());
        println!(
            "rules {} | unverified sources {} | error {:?}",
            report.added, report.unverified_sources, report.extraction_error
        );
        for fragment in &report.uncovered {
            let shown: String = fragment.chars().take(70).collect();
            println!("  uncovered: {shown:?}");
        }
        let found = store
            .search_notes(inocode_core::store::NoteQuery {
                text: query.to_string(),
                project: Some(project),
                include_shared: false,
                kinds: vec![inocode_core::store::NoteKind::Rule],
                usable_only: false,
                since: None,
                until: None,
                limit: 4,
                budget: Duration::from_secs(5),
            })
            .await
            .expect("search");
        for hit in found.items {
            let note = store.note(hit.id).await.expect("note").expect("exists");
            let source: String = note.body.chars().take(50).collect();
            println!(
                "  - {} | source: {source:?}",
                note.summary.lines().next().unwrap_or_default()
            );
        }
        drop(store);
        remove(&path);
    }
}
