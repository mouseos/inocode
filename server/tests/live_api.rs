//! Live check against the real provider. The `live-api` workflow job runs it on
//! Linux, Windows, and macOS with `LOGFARE_API_KEY` supplied by the repository
//! secret; it is ignored otherwise because it needs network access and a key.

mod support;

use std::sync::Arc;
use std::time::Instant;

use inocode_core::Core;
use serde_json::{json, Value};
use support::*;

#[tokio::test]
#[ignore = "requires LOGFARE_API_KEY; run by the live-api workflow job"]
async fn logfare_generates_and_streams_text_over_tcp() {
    with_timeout_of(LIVE_TIMEOUT, async {
        let core = Arc::new(Core::from_env());
        let (addr, _stop, _handle) = start_server(core).await;
        let mut client = connect(addr).await;

        let prompt = "Write two short sentences about the ocean in Japanese.";
        let started = Instant::now();
        client
            .1
            .write_json(
                1,
                &json!({"id": 1, "method": "chat", "params": {"prompt": prompt}}),
            )
            .await
            .expect("send chat");

        let mut deltas = 0usize;
        let mut text = String::new();
        let mut first_delta = None;
        let completed = loop {
            let (stream, event): (u64, Value) = client.0.read_json().await.expect("read event");
            assert_eq!(stream, 1);
            assert_eq!(event["id"], 1);
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
        };

        assert!(
            deltas >= 2,
            "the provider returned {deltas} delta(s), so streaming was not observed"
        );
        assert!(!text.trim().is_empty(), "the provider returned no text");

        println!(
            "live logfare/auto: deltas={deltas} chars={} first_delta={}ms total={}ms",
            text.chars().count(),
            first_delta.unwrap_or_default().as_millis(),
            completed.as_millis()
        );
    })
    .await;
}
