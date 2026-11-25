//! Live check against the real provider. The `live-api` workflow job runs it on
//! Linux, Windows, and macOS with the repository secret passed as the
//! test-only `INOCODE_TEST_API_KEY` (support::live_core keeps it in a
//! credential store of the test's own); it is ignored otherwise because it
//! needs network access and a key.

mod support;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use inocode_core::settings::Kind;
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
    min_deltas: usize,
) {
    assert!(
        deltas >= min_deltas,
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
#[ignore = "requires INOCODE_TEST_API_KEY; run by the live-api workflow job"]
async fn logfare_generates_and_streams_text_over_tcp() {
    with_timeout_of(LIVE_TIMEOUT, async {
        let database = live_folder("api-chat").join("inocode.db");
        let core = Arc::new(live_core(&database, None).await);
        let (addr, _stop, _handle) = start_server(core).await;
        let mut client = connect(addr).await;

        let prompt = "Write two short sentences about the ocean in Japanese.";
        let (deltas, text, first_delta, completed, _kinds) = run_chat(&mut client, 1, prompt).await;
        print_result("logfare/auto", deltas, &text, first_delta, completed, 2);
    })
    .await;
}

/// A turn of a thread that runs on the settings' default, answering a
/// question whose answer is known.
#[tokio::test]
#[ignore = "requires a provider credential; run by hand"]
async fn a_turn_answers_on_the_provider_the_settings_choose() {
    with_timeout_of(LIVE_TIMEOUT, async {
        let database = live_folder("api-turn").join("inocode.db");
        let core = Arc::new(live_core(&database, None).await);
        let (addr, _stop, _handle) = start_server(core).await;
        let mut client = connect(addr).await;
        let thread = call(&mut client, 1, json!({"id": 1, "method": "thread/start"})).await
            ["result"]["thread_id"]
            .as_i64()
            .expect("thread id");
        let chosen = call(
            &mut client,
            2,
            json!({"id": 2, "method": "thread/settings", "params": {"thread_id": thread}}),
        )
        .await;
        client
            .1
            .write_json(
                3,
                &json!({"id": 3, "method": "turn/start", "params": {
                    "thread_id": thread,
                    "prompt": "What is 2 + 3? Reply with the number only.",
                }}),
            )
            .await
            .expect("send");
        let mut text = String::new();
        let started = Instant::now();
        let last = loop {
            let event = next_message(&mut client, 3).await;
            match event["event"].as_str().unwrap_or_default() {
                "delta" => text.push_str(event["text"].as_str().unwrap_or_default()),
                "completed" | "failed" | "cancelled" => break event,
                _ => {}
            }
        };
        assert_eq!(last["event"], "completed", "{last}");
        assert!(text.contains('5'), "the answer was {text:?}");
        println!(
            "live turn: provider={} model={} reason={} answer={:?} total={}ms",
            chosen["result"]["provider"],
            chosen["result"]["model"],
            last["reason"],
            text.trim(),
            started.elapsed().as_millis()
        );
    })
    .await;
}

#[tokio::test]
#[ignore = "requires a provider credential; run by the live-api workflow jobs"]
async fn logfare_messages_api_generates_and_streams_text_over_tcp() {
    with_timeout_of(LIVE_TIMEOUT, async {
        // The provider is selected through the settings, as a deployment
        // does; the base URL and model stay the kind's unless the test
        // variables name others.
        let database = live_folder("api-messages").join("inocode.db");
        let core = Arc::new(live_core(&database, Some(Kind::Anthropic)).await);
        let (addr, _stop, _handle) = start_server(core).await;
        let mut client = connect(addr).await;

        // A longer answer gives the provider a chance to deliver the text in
        // several deltas; the diagnostic line reports only counts.
        let prompt =
            "Write a story about the ocean in Japanese, at least 300 words, with paragraphs.";
        let (deltas, text, first_delta, completed, kinds) = run_chat(&mut client, 1, prompt).await;
        let model_name = live_model(Some(Kind::Anthropic));
        println!(
            "live {model_name} messages kinds: {}",
            kinds
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(",")
        );
        // The Anthropic spec does not guarantee a minimum number of deltas;
        // an answer that arrives whole in one text_delta is valid (measured on
        // Logfare and recorded in docs/evidence/anthropic-messages.md). Judge
        // answer validity, not the delta count; sequential delivery is proven
        // by the mock tests that split events at arbitrary byte boundaries.
        print_result(
            &format!("{model_name} messages"),
            deltas,
            &text,
            first_delta,
            completed,
            1,
        );
    })
    .await;
}

/// Diagnostic for the Messages path: reads the provider's SSE stream directly
/// and prints which event types and delta types arrive. Generated text is never
/// printed.
#[tokio::test]
#[ignore = "requires INOCODE_TEST_API_KEY; run by the live-api workflow job"]
async fn logfare_messages_stream_event_kinds() {
    with_timeout_of(LIVE_TIMEOUT, async {
        let live = live_provider(Some(Kind::Anthropic));
        let api_key = live.api_key.clone().expect("INOCODE_TEST_API_KEY");
        let base = live.provider.base_url().to_string();
        let model = live_model(Some(Kind::Anthropic));
        let client = reqwest::Client::new();
        // Same URL convention as the implementation: the official Anthropic
        // client receives a base without /v1 and appends /v1/messages.
        let messages_url = {
            let base = base.trim_end_matches('/');
            let base = base.strip_suffix("/v1").unwrap_or(base);
            format!("{base}/v1/messages")
        };
        let prompt =
            "Write a story about the ocean in Japanese, at least 300 words, with paragraphs.";

        // One request per case. `thinking: {"type":"disabled"}` is the official
        // parameter for turning thinking off; whether this provider honors it
        // is what this diagnostic measures. Only the configured model is
        // probed: the CI free tier answers extra model probes with 429.
        let cases: &[(&str, &str, Value)] = &[
            ("model=auto/default", &model, json!({})),
            (
                "model=auto/thinking-disabled",
                &model,
                json!({ "thinking": { "type": "disabled" } }),
            ),
        ];
        for (label, case_model, extra) in cases {
            let mut body = json!({
                "model": case_model,
                "max_tokens": inocode_core::DEFAULT_MAX_TOKENS,
                "messages": [{ "role": "user", "content": prompt }],
                "stream": true,
            });
            let object = body.as_object_mut().expect("body is an object");
            for (key, value) in extra.as_object().expect("variant is an object") {
                object.insert(key.clone(), value.clone());
            }

            let mut response = client
                .post(&messages_url)
                .header("x-api-key", &api_key)
                .header("anthropic-version", "2023-06-01")
                .header("accept", "text/event-stream")
                .json(&body)
                .send()
                .await
                .expect("send request");
            println!(
                "live messages diagnostic [{label}]: status={} content_type={:?}",
                response.status(),
                response
                    .headers()
                    .get("content-type")
                    .and_then(|value| value.to_str().ok())
            );
            if !response.status().is_success() {
                continue;
            }

            let mut event_types: BTreeMap<String, usize> = BTreeMap::new();
            let mut delta_types: BTreeMap<String, usize> = BTreeMap::new();
            let mut block_types: BTreeMap<String, usize> = BTreeMap::new();
            let mut data = String::new();
            let mut dispatch = |payload: &str| {
                let value: Value = match serde_json::from_str(payload) {
                    Ok(value) => value,
                    Err(_) => {
                        *event_types.entry("<invalid json>".to_string()).or_default() += 1;
                        return;
                    }
                };
                let kind = value
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("<no type>")
                    .to_string();
                *event_types.entry(kind.clone()).or_default() += 1;
                if kind == "content_block_delta" {
                    let delta = value
                        .get("delta")
                        .and_then(|delta| delta.get("type"))
                        .and_then(Value::as_str)
                        .unwrap_or("<no delta type>")
                        .to_string();
                    *delta_types.entry(delta).or_default() += 1;
                }
                if kind == "content_block_start" {
                    let block = value
                        .get("content_block")
                        .and_then(|block| block.get("type"))
                        .and_then(Value::as_str)
                        .unwrap_or("<no block type>")
                        .to_string();
                    *block_types.entry(block).or_default() += 1;
                }
            };
            // Lines can span chunk boundaries; a partial tail is carried over
            // instead of being treated as a complete line (which mis-splits a
            // data payload mid-JSON and miscounts events).
            let mut line_buf = String::new();
            while let Some(chunk) = response.chunk().await.expect("read chunk") {
                line_buf.push_str(&String::from_utf8_lossy(&chunk));
                let lines: Vec<String> = line_buf.split('\n').map(str::to_string).collect();
                let last = lines.len() - 1;
                for (index, line) in lines.into_iter().enumerate() {
                    if index == last {
                        line_buf = line;
                        break;
                    }
                    let line = line.trim_end_matches('\r');
                    if line.is_empty() {
                        if !data.is_empty() {
                            dispatch(&data);
                            data.clear();
                        }
                    } else if let Some(rest) = line.strip_prefix("data:") {
                        if !data.is_empty() {
                            data.push('\n');
                        }
                        data.push_str(rest.trim_start_matches(' '));
                    }
                    // Lines other than `data:` and blank lines carry no counts.
                }
            }
            // A final line without a terminator still completes an event.
            let tail = std::mem::take(&mut line_buf);
            let tail = tail.trim_end_matches('\r');
            if !tail.is_empty() {
                if let Some(rest) = tail.strip_prefix("data:") {
                    if !data.is_empty() {
                        data.push('\n');
                    }
                    data.push_str(rest.trim_start_matches(' '));
                }
            }
            if !data.is_empty() {
                dispatch(&data);
            }

            let show = |name: &str, counts: &BTreeMap<String, usize>| {
                println!(
                    "live messages diagnostic [{label}] {name}: {}",
                    counts
                        .iter()
                        .map(|(k, v)| format!("{k}={v}"))
                        .collect::<Vec<_>>()
                        .join(",")
                );
            };
            show("event types", &event_types);
            show("delta types", &delta_types);
            show("block types", &block_types);
        }
    })
    .await;
}
