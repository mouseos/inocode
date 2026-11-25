//! A delegated task and a review against the provider the test-only
//! `INOCODE_TEST_*` variables name (support::live_core), the same model as
//! the lead and the member. Ignored because it needs network access and a
//! credential; run it by hand (docs/evidence/company.md).

mod support;

use inocode_core::turn;
use serde_json::json;
use tokio::sync::{mpsc, watch};

#[tokio::test]
#[ignore = "requires a provider credential; run by hand"]
async fn the_lead_delegates_a_small_task_and_asks_for_a_review() {
    let folder = support::live_folder("company");
    let project = folder.join("project");
    std::fs::create_dir_all(&project).expect("project");
    inocode_core::sandbox::configure(true, Some(env!("CARGO_BIN_EXE_inocode-server").into()));
    let core = support::live_core(&folder.join("inocode.db"), None).await;
    let model = support::live_model(None);
    core.add_member(&json!({
        "name": "helper", "provider": support::LIVE_PROVIDER_ID, "model": model,
        "role": "implements small, simple changes",
    }))
    .await
    .expect("member");
    core.set_allow(&json!({"default": ["helper"]}))
        .await
        .expect("allow");

    let thread = core
        .start_thread(Some(&project.to_string_lossy()))
        .await
        .expect("thread");
    let claim = core.begin_turn(thread).await.expect("turn");
    let (_cancel_sender, cancel) = watch::channel(false);
    let (events, mut received) = mpsc::channel(1024);
    let printer = tokio::spawn(async move {
        while let Some(event) = received.recv().await {
            if let turn::TurnEvent::Message { seq, message } = event {
                println!(
                    "--- {seq}: {}",
                    serde_json::to_string(&message.body).unwrap()
                );
            }
        }
    });
    let outcome = core
        .run_turn(
            thread,
            "Use the delegate tool to have the member helper create hello.txt in the project \
             folder containing the single word hello. Then use the review tool to have \
             hello.txt reviewed. Then answer in one line."
                .to_string(),
            None,
            turn::Approvals::default(),
            claim,
            cancel,
            events,
        )
        .await;
    core.end_turn(thread);
    let _ = printer.await;

    println!("ending: {:?}", outcome.ending);
    println!("warnings: {:?}", outcome.warnings);
    println!(
        "usage: {} requests, {} in ({} cached), {} out",
        outcome.usage.requests, outcome.usage.input, outcome.usage.cached, outcome.usage.output
    );
    println!(
        "hello.txt: {:?}",
        std::fs::read_to_string(project.join("hello.txt")).ok()
    );
    let store = core.thread_store().expect("store");
    for (child, member, task, _) in store.delegations(thread).await.expect("delegations") {
        let usage = store.session_usage(child).await.expect("usage");
        println!(
            "delegated to {member} in thread {}: {task:?}; {} requests, {} in, {} out",
            child.0, usage.requests, usage.input, usage.output
        );
    }
}
