//! A goal run against the provider configured in the environment
//! (`INOCODE_LLM_PROVIDER`, `INOCODE_LLM_MODEL`, …). Ignored because it
//! needs network access and a credential; run it by hand.

use inocode_core::store::Store;
use inocode_core::{turn, Core};
use tokio::sync::{mpsc, watch};

#[tokio::test]
#[ignore = "requires a provider credential; run by hand"]
async fn a_goal_runs_until_the_checker_decides() {
    let folder = std::env::temp_dir().join(format!("inocode-live-goal-{}", std::process::id()));
    let project = folder.join("project");
    std::fs::create_dir_all(&project).expect("project");
    std::fs::write(project.join("numbers.txt"), "17\n25\n8\n-3\n41\n").expect("numbers");
    // Commands run through the server's helper, not through this test binary.
    inocode_core::sandbox::configure(true, Some(env!("CARGO_BIN_EXE_inocode-server").into()));
    let store = Store::open(folder.join("inocode.db")).await.expect("store");
    let core = Core::from_env().with_store(store);

    let thread = core
        .start_thread(Some(&project.to_string_lossy()))
        .await
        .expect("thread");
    let objective = "In the project folder, sum.txt exists and its only line is the sum of \
                     the numbers in numbers.txt.";
    core.set_goal(thread, objective, None, Some(6), Some(400_000))
        .await
        .expect("goal");

    let steer = core.begin_turn(thread).await.expect("turn");
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
            "Work on the goal.".to_string(),
            None,
            turn::Approvals::default(),
            steer,
            cancel,
            events,
        )
        .await;
    core.end_turn(thread);
    let _ = printer.await;

    println!("ending: {:?}", outcome.ending);
    println!(
        "usage: {} requests, {} in, {} out",
        outcome.usage.requests, outcome.usage.input, outcome.usage.output
    );
    let goal = core.goal(thread).await.expect("read").expect("goal");
    println!("goal: {}", serde_json::to_string_pretty(&goal).unwrap());
    println!(
        "sum.txt: {:?}",
        std::fs::read_to_string(project.join("sum.txt")).ok()
    );
}
