//! Release-mode measurement of the chat delta path through the server.
//!
//! Measurements are not assertions: they print numbers for the report. Run
//! them single-threaded in a release build:
//!
//! ```text
//! cargo test --release -p inocode-server --test measure_server -- --ignored --nocapture --test-threads=1
//! ```

mod support;

use std::sync::Arc;
use std::time::Instant;

use inocode_core::{Core, Llm, LlmConfig, ProviderKind};
use serde_json::{json, Value};
use support::*;

const DELTAS: usize = 20_000;
const ROUNDS: usize = 7;

/// The provider body cut into 64 KiB writes, as a fast upstream would send it.
fn provider_chunks() -> Vec<Vec<u8>> {
    let mut body = Vec::new();
    for index in 0..DELTAS {
        body.extend_from_slice(&sse_delta(&format!("word{index} ")));
    }
    body.extend_from_slice(&sse_done());
    body.chunks(64 * 1024).map(<[u8]>::to_vec).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "measurement: run with --release --ignored --nocapture --test-threads=1"]
async fn chat_delta_forwarding() {
    let chunks = provider_chunks();
    let mock = start_mock(move |_request: &MockRequest| MockReply::stream(chunks.clone())).await;
    let llm = Llm::new(LlmConfig {
        provider: ProviderKind::OpenAi,
        auth: inocode_core::oauth::AuthKind::ApiKey,
        base_url: mock.base_url.clone(),
        model: "measure-model".to_string(),
        api_key: "test-key".to_string(),
        auth_store_path: None,
    })
    .expect("build the provider client");
    let (addr, _stop, _server) = start_server(Arc::new(Core::with_llm(llm))).await;
    let mut client = connect(addr).await;

    let mut samples = Vec::with_capacity(ROUNDS);
    for round in 0..=ROUNDS {
        let started = Instant::now();
        client
            .1
            .write_json(
                1,
                &json!({"id": round, "method": "chat", "params": {"prompt": "go"}}),
            )
            .await
            .expect("send chat");
        let mut deltas = 0usize;
        loop {
            let (_, event): (u64, Value) = client.0.read_json().await.expect("read event");
            match event["event"].as_str() {
                Some("delta") => deltas += 1,
                Some("started") => {}
                Some("completed") => break,
                other => panic!("unexpected event {other:?}: {event}"),
            }
        }
        assert_eq!(deltas, DELTAS);
        // Round 0 warms up the connections and allocator.
        if round > 0 {
            samples.push(started.elapsed().as_secs_f64());
        }
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = samples[ROUNDS / 2];
    println!(
        "\n== chat delta forwarding ({DELTAS} deltas, mock provider -> server -> TCP client) =="
    );
    println!(
        "median {:.2} ms  min {:.2} ms  {:.0} ns/delta",
        median * 1e3,
        samples[0] * 1e3,
        median * 1e9 / DELTAS as f64
    );
}
