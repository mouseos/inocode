//! Live check against the real provider. The `live-api` workflow job runs it on
//! Linux, Windows, and macOS with `LOGFARE_API_KEY` supplied by the repository
//! secret; it is ignored otherwise because it needs network access and a key.

mod support;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use inocode_core::Core;
use serde_json::{json, Value};
use support::*;

/// Sends one chat request and collects the streamed answer.
async fn run_chat(
    client: &mut Client,
    stream_id: u64,
    prompt: &str,
) -> (
    usize,
    String,
    std::time::Duration,
    std::time::Duration,
    BTreeMap<String, usize>,
) {
    let started = Instant::now();
    client
        .1
        .write_json(
            stream_id,
            &json!({"id": stream_id, "method": "chat", "params": {"prompt": prompt}}),
        )
        .await
        .expect("send chat");

    let mut deltas = 0usize;
    let mut text = String::new();
    let mut first_delta = None;
    let mut kinds: BTreeMap<String, usize> = BTreeMap::new();
    let completed = loop {
        let (stream, event): (u64, Value) = client.0.read_json().await.expect("read event");
        assert_eq!(stream, stream_id);
        assert_eq!(event["id"], stream_id);
        match event["event"].as_str().unwrap_or_default() {
            "started" => {}
            "delta" => {
                if first_delta.is_none() {
                    first_delta = Some(started.elapsed());
                }
                deltas += 1;
                text.push_str(event["text"].as_str().unwrap_or_default());
            }
            "failed" | "cancelled" => panic!("the live generation ended with {event}"),
            "completed" => break started.elapsed(),
            other => panic!("unexpected live event {other}"),
        }
        *kinds
            .entry(event["event"].as_str().unwrap_or_default().to_string())
            .or_default() += 1;
    };
    (
        deltas,
        text,
        first_delta.unwrap_or_default(),
        completed,
        kinds,
    )
}

fn print_result(
    name: &str,
    deltas: usize,
    text: &str,
    first_delta: std::time::Duration,
    completed: std::time::Duration,
) {
    assert!(
        deltas >= 2,
        "the provider returned {deltas} delta(s), so streaming was not observed"
    );
    assert!(!text.trim().is_empty(), "the provider returned no text");
    println!(
        "live {name}: deltas={deltas} chars={} first_delta={}ms total={}ms",
        text.chars().count(),
        first_delta.as_millis(),
        completed.as_millis()
    );
}

#[tokio::test]
#[ignore = "requires LOGFARE_API_KEY; run by the live-api workflow job"]
async fn logfare_generates_and_streams_text_over_tcp() {
    with_timeout_of(LIVE_TIMEOUT, async {
        let core = Arc::new(Core::from_env());
        let (addr, _stop, _handle) = start_server(core).await;
        let mut client = connect(addr).await;

        let prompt = "Write two short sentences about the ocean in Japanese.";
        let (deltas, text, first_delta, completed, _kinds) = run_chat(&mut client, 1, prompt).await;
        print_result("logfare/auto", deltas, &text, first_delta, completed);
    })
    .await;
}

#[tokio::test]
#[ignore = "requires LOGFARE_API_KEY; run by the live-api workflow job"]
async fn logfare_messages_api_generates_and_streams_text_over_tcp() {
    with_timeout_of(LIVE_TIMEOUT, async {
        // The provider must be selected through the same environment variable
        // the deployment uses, and the base URL and model stay the defaults.
        std::env::set_var("INOCODE_LLM_PROVIDER", "anthropic");
        let core = Arc::new(Core::from_env());
        let (addr, _stop, _handle) = start_server(core).await;
        let mut client = connect(addr).await;

        // A longer answer gives the provider a chance to deliver the text in
        // several deltas; the diagnostic line reports only counts.
        let prompt =
            "Write a story about the ocean in Japanese, at least 300 words, with paragraphs.";
        let (deltas, text, first_delta, completed, kinds) = run_chat(&mut client, 1, prompt).await;
        println!(
            "live logfare/auto messages kinds: {}",
            kinds
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(",")
        );
        print_result(
            "logfare/auto messages",
            deltas,
            &text,
            first_delta,
            completed,
        );
    })
    .await;
}
