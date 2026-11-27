//! A turn that has to find an account connector's tool with `tool_search`
//! and call it, on the default provider of `~/.inocode/settings.json`.
//! Ignored because it needs network access and a signed-in account; run it
//! by hand.

use inocode_core::store::Store;
use inocode_core::{turn, Core};
use tokio::sync::{mpsc, watch};

#[tokio::test]
#[ignore = "requires a signed-in account; run by hand"]
async fn a_deferred_connector_tool_is_found_and_called() {
    // The store sits beside ~/.inocode/auth.json so the signed-in accounts'
    // connectors are reached; it is removed afterwards.
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .expect("home");
    let database = std::path::PathBuf::from(home)
        .join(".inocode")
        .join(format!("live-tool-search-{}.db", std::process::id()));
    let project = std::env::temp_dir().join(format!("inocode-live-search-{}", std::process::id()));
    std::fs::create_dir_all(&project).expect("project");
    let store = Store::open(&database).await.expect("store");
    let core = Core::new().with_store(store);
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
                let mut line = serde_json::to_string(&message.body).unwrap();
                if line.len() > 600 {
                    let mut cut = 600;
                    while !line.is_char_boundary(cut) {
                        cut -= 1;
                    }
                    line.truncate(cut);
                    line.push('…');
                }
                println!("--- {seq}: {line}");
            }
        }
    });
    let outcome = core
        .run_turn(
            thread,
            turn::Prompt {
                text: "Using my Notion connector, find out my Notion user name and tell me it. \
             Do not read or change any Notion page."
                    .to_string(),
                ..Default::default()
            },
            turn::Approvals::default(),
            claim,
            cancel,
            events,
        )
        .await;
    core.end_turn(thread);
    let _ = printer.await;
    drop(core);

    println!("ending: {:?}", outcome.ending);
    println!(
        "usage: {} requests, {} in, {} out",
        outcome.usage.requests, outcome.usage.input, outcome.usage.output
    );
    let _ = std::fs::remove_file(&database);
    let _ = std::fs::remove_dir_all(database.with_extension("files"));
    let _ = std::fs::remove_dir_all(&project);
}
