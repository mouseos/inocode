//! Rule import as verbatim chunks with model-written search sentences,
//! against a local stand-in for a Chat Completions provider.

mod support;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use inocode_core::rules::Chunking;
use inocode_core::store::Store;
use inocode_core::{Llm, LlmConfig, ProviderKind};
use serde_json::json;
use support::*;

const DOC: &str = "Aの場合はBする。Cの時はやらない。pushの前に必ず確認を取ること。コミットメッセージは英語で書く。";
const SMALL: Chunking = Chunking {
    chars: 20,
    overlap: 5,
};

fn llm(mock: &MockApi) -> Arc<Llm> {
    Arc::new(
        Llm::new(LlmConfig {
            provider: ProviderKind::OpenAi,
            auth: inocode_core::oauth::AuthKind::ApiKey,
            base_url: mock.base_url.clone(),
            model: "writer".to_string(),
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

/// Writes one sentence per chunk in the request ("chunk N requires
/// approval"), or a broken answer while `broken` is set.
fn writer(
    calls: Arc<AtomicUsize>,
    broken: Arc<AtomicBool>,
) -> impl Fn(&MockRequest) -> MockReply + Send + Sync + 'static {
    move |request: &MockRequest| {
        calls.fetch_add(1, Ordering::SeqCst);
        let prompt = request.body["messages"][1]["content"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let answer = if broken.load(Ordering::SeqCst) {
            "sorry".to_string()
        } else {
            let count = prompt.matches("<<<CHUNK ").count();
            let chunks: Vec<_> = (1..=count)
                .map(|id| json!({"id": id, "rules": [format!("chunk {id} requires approval")]}))
                .collect();
            json!({ "chunks": chunks }).to_string()
        };
        MockReply::stream(vec![sse_delta(&answer), sse_done()])
    }
}

#[tokio::test]
async fn chunks_are_kept_verbatim_and_sentences_widen_search() {
    let calls = Arc::new(AtomicUsize::new(0));
    let broken = Arc::new(AtomicBool::new(true));
    let mock = start_mock(writer(Arc::clone(&calls), Arc::clone(&broken))).await;
    let llm = llm(&mock);
    let path = database();
    let store = Store::open(&path).await.expect("store");
    let project = store.project("p").await.expect("project");
    let import = |text: String| {
        let (store, llm) = (store.clone(), Arc::clone(&llm));
        async move {
            store
                .import_rules(Some(llm), Some(project), "AGENTS.md", &text, SMALL)
                .await
                .expect("import")
        }
    };

    let first = import(DOC.to_string()).await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "one request for the document"
    );
    assert_eq!(first.added, 4);
    assert!(first.keys_error.is_some(), "a broken answer is reported");
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
    assert!(search("requires").await.is_empty(), "no sentences yet");
    assert_eq!(
        search("Cの時").await.len(),
        1,
        "the verbatim text is searchable"
    );

    broken.store(false, Ordering::SeqCst);
    let second = import(DOC.to_string()).await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "chunks without sentences are retried"
    );
    assert_eq!((second.unchanged, second.added), (4, 0));
    assert!(second.keys_error.is_none());
    let found = search("requires").await;
    assert_eq!(found.len(), 4, "the sentences are search keys");
    let note = store
        .note(found[0].id)
        .await
        .expect("note")
        .expect("exists");
    assert!(
        DOC.contains(&note.body),
        "the stored rule is the source text"
    );

    let third = import(DOC.to_string()).await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "an unchanged document is not resent"
    );
    assert_eq!(third.unchanged, 4);

    let edited = format!("{DOC}テストは必ず実行する。");
    let fourth = import(edited).await;
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    assert!(fourth.added >= 1, "the new text is a new chunk");
    assert!(fourth.unchanged >= 2, "chunks before the edit are kept");

    drop(store);
    remove(&path);
}

/// Imports this repository's AGENTS.md through the provider configured in
/// the environment and prints the result. Ignored because it needs network
/// access and a credential; run it by hand.
#[tokio::test]
#[ignore = "requires a provider credential; run by hand"]
async fn live_import_of_this_repository_agents_md() {
    let markdown = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../AGENTS.md"))
        .expect("AGENTS.md");
    let llm = Arc::new(Llm::from_env().expect("provider"));
    let path = database();
    let store = Store::open(&path).await.expect("store");
    let project = store.project("inocode").await.expect("project");
    let started = std::time::Instant::now();
    let report = store
        .import_rules(
            Some(llm),
            Some(project),
            "AGENTS.md",
            &markdown,
            Chunking::default(),
        )
        .await
        .expect("import");
    println!("elapsed: {:?}", started.elapsed());
    println!("chunks: {}", report.added);
    println!("search sentences error: {:?}", report.keys_error);
    for query in ["コミット", "研究根拠", "変更履歴を書かない"] {
        let found = store
            .rules_for(Some(project), query, 2, Duration::from_secs(5), None)
            .await
            .expect("rules");
        for hit in found.items {
            let note = store.note(hit.id).await.expect("note").expect("exists");
            let sentences: Vec<&str> = note.summary.lines().take(3).collect();
            println!(
                "  [{query}] note {} | {} sentences, e.g. {sentences:?}",
                hit.id.0,
                note.summary.lines().count()
            );
        }
    }
    drop(store);
    remove(&path);
}
