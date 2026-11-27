//! A turn that has to find a plugin in the Claude Code and Codex catalogs and
//! install it, in which the person approves the install when asked, against the provider the test-only `INOCODE_TEST_*` variables
//! name (support::live_core). Ignored because it needs network access and a
//! signed-in account; run it by hand.

mod support;

use inocode_core::store::SessionId;
use inocode_core::{turn, Core};
use serde_json::{json, Value};
use tokio::sync::{mpsc, watch};

/// Runs a turn that approves the installs it asks about and refuses the
/// rest; the approval prompts it showed.
async fn run(core: &Core, thread: SessionId, prompt: &str) -> (turn::Outcome, Vec<Value>) {
    let claim = core.begin_turn(thread).await.expect("turn");
    let (_cancel_sender, cancel) = watch::channel(false);
    let (events, mut received) = mpsc::channel(1024);
    let printer = async {
        let mut asked = Vec::new();
        while let Some(event) = received.recv().await {
            match event {
                turn::TurnEvent::Message { seq, message } => {
                    let mut line = serde_json::to_string(&message.body).unwrap();
                    if line.len() > 800 {
                        let mut cut = 800;
                        while !line.is_char_boundary(cut) {
                            cut -= 1;
                        }
                        line.truncate(cut);
                        line.push('…');
                    }
                    println!("--- {seq}: {line}");
                }
                turn::TurnEvent::Elicitation(question) => {
                    println!("asked: {question}");
                    let action = match question["tool"] == "plugin_install" {
                        true => "accept",
                        false => "decline",
                    };
                    let id = question["id"].as_u64().expect("id");
                    core.answer(thread, id, json!({ "action": action }));
                    asked.push(question);
                }
                _ => {}
            }
        }
        asked
    };
    let run = core.run_turn(
        thread,
        prompt.to_string(),
        None,
        turn::Approvals::default(),
        claim,
        cancel,
        events,
    );
    let (outcome, asked) = tokio::join!(run, printer);
    core.end_turn(thread);
    println!("ending: {:?}", outcome.ending);
    println!(
        "usage: {} requests, {} in, {} out",
        outcome.usage.requests, outcome.usage.input, outcome.usage.output
    );
    (outcome, asked)
}

#[tokio::test]
#[ignore = "requires a signed-in account; run by hand"]
async fn a_plugin_is_found_and_installed_after_approval() {
    let root = std::env::temp_dir().join(format!("inocode-live-plugins-{}", std::process::id()));
    let project = root.join("project");
    std::fs::create_dir_all(&project).expect("project");
    let core = support::live_core(&root.join("inocode.db"), None).await;
    let thread = core
        .start_thread(Some(&project.to_string_lossy()))
        .await
        .expect("thread");

    let (_, asked) = run(
        &core,
        thread,
        "Find a plugin that helps write git commit messages and install the one that fits best.",
    )
    .await;
    assert!(
        asked
            .iter()
            .any(|question| question["tool"] == "plugin_install"),
        "the install waits for the person"
    );
    drop(core);

    let installed = root.join("plugins").join("installed");
    for catalog in std::fs::read_dir(&installed).expect("installed") {
        let catalog = catalog.expect("entry").path();
        for plugin in std::fs::read_dir(&catalog).expect("catalog") {
            println!("installed: {}", plugin.expect("entry").path().display());
        }
    }
    let _ = std::fs::remove_dir_all(&root);
}
