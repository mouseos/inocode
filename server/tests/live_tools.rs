//! Live tool-calling round trip against the provider configured in the
//! environment (`INOCODE_LLM_PROVIDER`, `INOCODE_LLM_MODEL`, …). Ignored
//! because it needs network access and a credential; run it by hand.

use inocode_core::conversation::{
    AssistantPart, Body, Conversation, Message, Speaker, StopReason, ToolSpec,
};
use inocode_core::Llm;
use serde_json::{json, Value};

mod support;

fn message(id: usize, body: Body) -> Message {
    Message {
        id: id.to_string(),
        body,
    }
}

/// Streams one turn and returns its text and the assembled turn.
async fn turn(llm: &Llm, conversation: &Conversation, tools: &[ToolSpec]) -> (String, Body) {
    let mut stream = llm.converse(conversation, tools).await.expect("start");
    let mut text = String::new();
    while let Some(delta) = stream.next().await.expect("stream") {
        text.push_str(&delta);
    }
    (text, stream.assistant())
}

#[tokio::test]
#[ignore = "requires a provider credential; run by hand"]
async fn a_tool_call_and_its_result_round_trip() {
    let llm = Llm::from_env().expect("provider");
    let tools = [ToolSpec {
        name: "get_weather".to_string(),
        description: "Returns the current weather for a city.".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {"city": {"type": "string"}},
            "required": ["city"],
        }),
    }];
    let mut conversation = Conversation {
        system: Some("Answer with the tools when they help.".to_string()),
        messages: vec![message(
            0,
            Body::User {
                from: Speaker::Person,
                parts: vec!["Work out which city hosted the 1964 Summer Olympics, then call get_weather for it.".to_string()],
            },
        )],
    };

    let (_, first) = turn(&llm, &conversation, &tools).await;
    let Body::Assistant { stop, parts, .. } = &first else {
        panic!("not an assistant turn");
    };
    let calls: Vec<(String, String)> = parts
        .iter()
        .filter_map(|part| match part {
            AssistantPart::ToolCall {
                call_id, arguments, ..
            } => Some((call_id.clone(), arguments.clone())),
            _ => None,
        })
        .collect();
    let reasoning = parts
        .iter()
        .filter(|part| matches!(part, AssistantPart::Reasoning { .. }))
        .count();
    let replayable = parts
        .iter()
        .filter(|part| {
            matches!(
                part,
                AssistantPart::Reasoning {
                    replay: Some(_),
                    ..
                }
            )
        })
        .count();
    println!(
        "first turn: stop {stop:?}, {} calls, {reasoning} reasoning parts ({replayable} with replay data)",
        calls.len()
    );
    assert!(!calls.is_empty(), "the model made no tool call");
    for (_, arguments) in &calls {
        let arguments: Value = serde_json::from_str(arguments).expect("JSON arguments");
        assert!(arguments["city"].is_string(), "arguments {arguments}");
    }

    conversation.messages.push(message(1, first.clone()));
    for (index, (call_id, _)) in calls.into_iter().enumerate() {
        conversation.messages.push(message(
            2 + index,
            Body::ToolResult {
                call_id,
                name: "get_weather".to_string(),
                content: "Sunny, 21 degrees Celsius.".to_string(),
                is_error: false,
                images: Vec::new(),
            },
        ));
    }

    let (text, second) = turn(&llm, &conversation, &tools).await;
    let Body::Assistant { stop, .. } = &second else {
        panic!("not an assistant turn");
    };
    println!("second turn: stop {stop:?}, {} chars", text.chars().count());
    assert_eq!(*stop, StopReason::Completed);
    assert!(!text.trim().is_empty(), "no answer after the tool result");
}

/// The thinking the provider shows streams as its own pieces, before the
/// answer, and the stored turn keeps it whole.
#[tokio::test]
#[ignore = "requires a provider credential; run by hand"]
async fn thinking_streams_before_the_answer() {
    let llm = Llm::from_env().expect("provider");
    let conversation = Conversation {
        system: None,
        messages: vec![message(
            0,
            Body::User {
                from: Speaker::Person,
                parts: vec!["A train leaves at 9:47 and the trip takes 2 hours 38 minutes.                     When does it arrive? Think it through, then give the time."
                    .to_string()],
            },
        )],
    };
    let mut stream = llm.converse(&conversation, &[]).await.expect("start");
    let (mut thinking, mut text, mut thinking_after_text) = (String::new(), String::new(), false);
    let mut pieces = (0, 0);
    while let Some(piece) = stream.next_piece().await.expect("stream") {
        match piece {
            inocode_core::Piece::Thinking(delta) => {
                thinking_after_text |= !text.is_empty();
                thinking.push_str(&delta);
                pieces.0 += 1;
            }
            inocode_core::Piece::Text(delta) => {
                text.push_str(&delta);
                pieces.1 += 1;
            }
        }
    }
    println!("thinking pieces: {}, text pieces: {}", pieces.0, pieces.1);
    println!(
        "thinking: {}",
        thinking.chars().take(300).collect::<String>()
    );
    println!("answer: {text}");
    assert!(!thinking.is_empty(), "no thinking was streamed");
    assert!(!thinking_after_text, "thinking came after the answer");
    let Body::Assistant { parts, .. } = stream.assistant() else {
        panic!("not an assistant turn");
    };
    assert!(parts
        .iter()
        .any(|part| matches!(part, AssistantPart::Reasoning { text, .. } if !text.is_empty())));
}

/// Over the protocol, a turn's `thinking` events reach the client before its
/// `delta` events.
#[tokio::test]
#[ignore = "requires a provider credential; run by hand"]
async fn thinking_reaches_the_client_before_the_answer() {
    support::with_timeout_of(support::LIVE_TIMEOUT, async {
        let root = std::env::temp_dir().join(format!("inocode-live-think-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("folder");
        let store = inocode_core::store::Store::open(&root.join("inocode.db"))
            .await
            .expect("store");
        let core = std::sync::Arc::new(inocode_core::Core::from_env().with_store(store));
        let (addr, _stop, _handle) = support::start_server(core).await;
        let mut client = support::connect(addr).await;
        let response = support::call(&mut client, 1, json!({"id": 1, "method": "thread/start"})).await;
        let thread = response["result"]["thread_id"].as_i64().expect("thread id");
        client
            .1
            .write_json(
                2,
                &json!({"id": 2, "method": "turn/start", "params": {"thread_id": thread,
                    "prompt": "A train leaves at 9:47 and the trip takes 2 hours 38 minutes.                                When does it arrive? Think it through, then give the time."}}),
            )
            .await
            .expect("send");
        let (mut thinking, mut text, mut thinking_after_text) = (String::new(), String::new(), false);
        let mut events = (0, 0);
        let ending = loop {
            let event = support::next_message(&mut client, 2).await;
            match event["event"].as_str().unwrap_or_default() {
                "thinking" => {
                    thinking_after_text |= !text.is_empty();
                    thinking.push_str(event["text"].as_str().unwrap_or_default());
                    events.0 += 1;
                }
                "delta" => {
                    text.push_str(event["text"].as_str().unwrap_or_default());
                    events.1 += 1;
                }
                kind @ ("completed" | "failed" | "cancelled") => break (kind.to_string(), event),
                _ => {}
            }
        };
        let _ = std::fs::remove_dir_all(&root);
        println!("thinking events: {}, delta events: {}", events.0, events.1);
        println!("thinking: {}", thinking.chars().take(300).collect::<String>());
        println!("answer: {text}");
        assert_eq!(ending.0, "completed", "{}", ending.1);
        assert!(!thinking.is_empty(), "no thinking event reached the client");
        assert!(!thinking_after_text, "thinking came after the answer");
    })
    .await;
}
