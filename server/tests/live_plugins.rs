//! A turn that has to find a plugin in the Claude Code and Codex catalogs and
//! ask to install it, then a turn in which the person approves the held
//! install, against the provider the test-only `INOCODE_TEST_*` variables
//! name (support::live_core). Ignored because it needs network access and a
//! signed-in account; run it by hand.

mod support;

use inocode_core::store::SessionId;
use inocode_core::{turn, Core};
use tokio::sync::{mpsc, watch};

async fn run(
    core: &Core,
    thread: SessionId,
    prompt: &str,
    approvals: turn::Approvals,
) -> turn::Outcome {
    let claim = core.begin_turn(thread).await.expect("turn");
    let (_cancel_sender, cancel) = watch::channel(false);
    let (events, mut received) = mpsc::channel(1024);
    let printer = tokio::spawn(async move {
        while let Some(event) = received.recv().await {
            if let turn::TurnEvent::Message { seq, message } = event {
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
        }
    });
    let outcome = core
        .run_turn(
            thread,
            prompt.to_string(),
            None,
            approvals,
            claim,
            cancel,
            events,
        )
        .await;
    core.end_turn(thread);
    let _ = printer.await;
    println!("ending: {:?}", outcome.ending);
    println!("held: {:?}", outcome.held);
    println!(
        "usage: {} requests, {} in, {} out",
        outcome.usage.requests, outcome.usage.input, outcome.usage.output
    );
    outcome
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

    let first = run(
        &core,
        thread,
        "Find a plugin that helps write git commit messages and install the one that fits best.",
        turn::Approvals::default(),
    )
    .await;
    let held = first
        .held
        .iter()
        .find(|held| held.tool == "plugin_install")
        .expect("the install waits for the person");
    let approvals = turn::Approvals {
        calls: vec![(held.tool.clone(), held.arguments.clone())],
        ..turn::Approvals::default()
    };
    run(&core, thread, "Approved. Install it.", approvals).await;
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
