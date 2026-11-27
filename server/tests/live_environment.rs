//! Requests that need the system's shell or today's date, each in its own
//! thread of one process, printing per request the shell calls, the ones a
//! shell refused as commands of another system, and the answer. Run once
//! with a build that tells the model its system, shell and time and once
//! with one that does not, to compare (docs/evidence/environment-context.md).
//! Against the provider the test-only `INOCODE_TEST_*` variables name
//! (support::live_core); ignored because it needs network access and a
//! signed-in account.

mod support;

use inocode_core::conversation::{AssistantPart, Body};
use inocode_core::turn;
use tokio::sync::{mpsc, watch};

const TASKS: [&str; 6] = [
    "List the three largest files in this project folder with their sizes in bytes.",
    "Count the total number of lines in all .txt files in this folder, including subfolders.",
    "Which files in this folder were changed in the last 24 hours?",
    "What is today's date and day of the week? Answer in the form YYYY-MM-DD Weekday.",
    "How many days are left until 2027-01-01?",
    "Show every line containing TODO in this folder, with its file and line number, using \
     a shell command.",
];

/// What a shell prints when it is given a command or option of another
/// system: PowerShell's and cmd's, and sh's.
const FOREIGN: [&str; 6] = [
    "CommandNotFoundException",
    "is not recognized as",
    "ParameterBindingException",
    "A parameter cannot be found",
    "PositionalParameterNotFound",
    "command not found",
];

fn project(root: &std::path::Path) -> std::path::PathBuf {
    let project = root.join("project");
    std::fs::create_dir_all(project.join("notes")).expect("project");
    std::fs::write(project.join("a.txt"), "one\ntwo TODO fix\nthree\n").expect("file");
    std::fs::write(project.join("notes").join("b.txt"), "x\n".repeat(40)).expect("file");
    std::fs::write(
        project.join("notes").join("c.txt"),
        "TODO write\nfine\n".repeat(5),
    )
    .expect("file");
    std::fs::write(project.join("big.log"), "log line\n".repeat(500)).expect("file");
    std::fs::write(project.join("data.bin"), vec![7u8; 2500]).expect("file");
    project
}

#[tokio::test]
#[ignore = "requires a signed-in account; run by hand"]
async fn requests_that_need_the_system_and_the_date() {
    let runs: usize = std::env::var("INOCODE_BENCH_RUNS").map_or(3, |runs| runs.parse().unwrap());
    let root = std::env::temp_dir().join(format!("inocode-live-env-{}", std::process::id()));
    let project = project(&root);
    let core = support::live_core(&root.join("inocode.db"), None).await;
    for run in 0..runs {
        for (task, prompt) in TASKS.iter().enumerate() {
            let thread = core
                .start_thread(Some(&project.to_string_lossy()))
                .await
                .expect("thread");
            let claim = core.begin_turn(thread).await.expect("turn");
            let (_cancel_sender, cancel) = watch::channel(false);
            let (events, mut received) = mpsc::channel(1024);
            let collector = tokio::spawn(async move {
                let (mut calls, mut foreign, mut answer) = (0, 0, String::new());
                while let Some(event) = received.recv().await {
                    let turn::TurnEvent::Message { message, .. } = event else {
                        continue;
                    };
                    match message.body {
                        Body::ToolResult { name, content, .. } if name == "shell" => {
                            calls += 1;
                            if FOREIGN.iter().any(|marker| content.contains(marker)) {
                                foreign += 1;
                                println!("    foreign: {}", first_line(&content));
                            }
                        }
                        Body::Assistant { parts, .. } => {
                            for part in parts {
                                match part {
                                    AssistantPart::Text { text } if !text.trim().is_empty() => {
                                        answer = text;
                                    }
                                    AssistantPart::ToolCall {
                                        name, arguments, ..
                                    } if name == "shell" => {
                                        println!("    shell: {}", first_line(&arguments));
                                    }
                                    _ => {}
                                }
                            }
                        }
                        _ => {}
                    }
                }
                (calls, foreign, answer)
            });
            let outcome = core
                .run_turn(
                    thread,
                    prompt.to_string(),
                    None,
                    turn::Approvals {
                        mode: turn::Mode::Full,
                    },
                    claim,
                    cancel,
                    events,
                )
                .await;
            core.end_turn(thread);
            let (calls, foreign, answer) = collector.await.expect("collector");
            println!(
                "RESULT run={run} task={task} ending={:?} requests={} input={} output={} \
                 shell={calls} foreign={foreign} answer={}",
                outcome.ending,
                outcome.usage.requests,
                outcome.usage.input,
                outcome.usage.output,
                serde_json::to_string(&answer).unwrap()
            );
        }
    }
    let _ = std::fs::remove_dir_all(&root);
}

fn first_line(text: &str) -> String {
    let line = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("");
    line.chars().take(200).collect()
}
