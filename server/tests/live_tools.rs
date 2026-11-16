//! Live tool-calling round trip against the provider configured in the
//! environment (`INOCODE_LLM_PROVIDER`, `INOCODE_LLM_MODEL`, …). Ignored
//! because it needs network access and a credential; run it by hand.

use inocode_core::conversation::{
    AssistantPart, Body, Conversation, Message, Speaker, StopReason, ToolSpec,
};
use inocode_core::Llm;
use serde_json::{json, Value};

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
