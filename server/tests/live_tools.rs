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

/// What one turn sent the client: the thinking, the answer and the
/// translated thinking, the number of each kind of event, whether thinking
/// came after answer text, and how the turn ended.
struct Seen {
    thinking: String,
    text: String,
    translated: String,
    pieces: (usize, usize, usize),
    thinking_after_text: bool,
}

const TRAIN: &str = "A train leaves at 9:47 and the trip takes 2 hours 38 minutes. When does it \
                     arrive? Think it through, then give the time.";

async fn seen_turn(client: &mut support::Client, stream_id: u64, thread: i64) -> Seen {
    client
        .1
        .write_json(
            stream_id,
            &json!({"id": stream_id, "method": "turn/start",
                "params": {"thread_id": thread, "prompt": TRAIN}}),
        )
        .await
        .expect("send");
    let mut seen = Seen {
        thinking: String::new(),
        text: String::new(),
        translated: String::new(),
        pieces: (0, 0, 0),
        thinking_after_text: false,
    };
    let ending = loop {
        let event = support::next_message(client, stream_id).await;
        let text = event["text"].as_str().unwrap_or_default().to_string();
        match event["event"].as_str().unwrap_or_default() {
            "thinking" => {
                seen.thinking_after_text |= !seen.text.is_empty();
                seen.thinking.push_str(&text);
                seen.pieces.0 += 1;
            }
            "delta" => {
                seen.text.push_str(&text);
                seen.pieces.1 += 1;
            }
            "thinking_translation" => {
                assert!(event["error"].is_null(), "{event}");
                seen.translated.push_str(&text);
                seen.pieces.2 += 1;
            }
            kind @ ("completed" | "failed" | "cancelled") => break (kind.to_string(), event),
            _ => {}
        }
    };
    println!(
        "thinking events: {}, delta events: {}, translation events: {}",
        seen.pieces.0, seen.pieces.1, seen.pieces.2
    );
    println!(
        "thinking: {}",
        seen.thinking.chars().take(300).collect::<String>()
    );
    println!(
        "translated: {}",
        seen.translated.chars().take(300).collect::<String>()
    );
    println!("answer: {}", seen.text);
    assert_eq!(ending.0, "completed", "{}", ending.1);
    seen
}

/// A server on the provider of the environment, with its database in a
/// folder of its own: the folder, a client and a thread, and the server's
/// stop handles.
async fn live_server(
    label: &str,
) -> (
    std::path::PathBuf,
    support::Client,
    i64,
    impl Sized,
    impl Sized,
) {
    let root = std::env::temp_dir().join(format!("inocode-live-{label}-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("folder");
    let store = inocode_core::store::Store::open(&root.join("inocode.db"))
        .await
        .expect("store");
    let core = std::sync::Arc::new(inocode_core::Core::from_env().with_store(store));
    let (addr, stop, handle) = support::start_server(core).await;
    let mut client = support::connect(addr).await;
    let response = support::call(&mut client, 1, json!({"id": 1, "method": "thread/start"})).await;
    let thread = response["result"]["thread_id"].as_i64().expect("thread id");
    (root, client, thread, stop, handle)
}

/// Over the protocol, a turn's `thinking` events reach the client before its
/// `delta` events.
#[tokio::test]
#[ignore = "requires a provider credential; run by hand"]
async fn thinking_reaches_the_client_before_the_answer() {
    support::with_timeout_of(support::LIVE_TIMEOUT, async {
        let (root, mut client, thread, _stop, _handle) = live_server("think").await;
        let seen = seen_turn(&mut client, 2, thread).await;
        let _ = std::fs::remove_dir_all(&root);
        assert!(
            !seen.thinking.is_empty(),
            "no thinking event reached the client"
        );
        assert!(!seen.thinking_after_text, "thinking came after the answer");
        assert_eq!(seen.pieces.2, 0, "translated with translation off");
    })
    .await;
}

/// Turned on with `settings/translation`, the thinking also reaches the
/// client translated into Japanese by `INOCODE_TRANSLATE_SERVICE` (`edge`
/// or `google`); turned off, it does not.
#[tokio::test]
#[ignore = "requires a provider credential and sends the thinking to a translation service; run by hand"]
async fn thinking_is_translated_while_turned_on() {
    support::with_timeout_of(support::LIVE_TIMEOUT, async {
        let service =
            std::env::var("INOCODE_TRANSLATE_SERVICE").unwrap_or_else(|_| "google".into());
        let (root, mut client, thread, _stop, _handle) = live_server("translate").await;
        let on = support::call(
            &mut client,
            3,
            json!({"id": 3, "method": "settings/translation",
                "params": {"thinking": true, "service": service, "to": "ja"}}),
        )
        .await;
        assert_eq!(on["result"]["thinking"], true, "{on}");
        let seen = seen_turn(&mut client, 5, thread).await;
        assert!(!seen.thinking.is_empty(), "no thinking");
        assert!(seen.pieces.2 > 0, "no translation reached the client");
        assert!(
            seen.translated
                .chars()
                .any(|c| ('\u{3040}'..='\u{30ff}').contains(&c)),
            "the translation has no Japanese kana"
        );
        let off = support::call(
            &mut client,
            7,
            json!({"id": 7, "method": "settings/translation", "params": {"thinking": false}}),
        )
        .await;
        assert_eq!(off["result"]["thinking"], false, "{off}");
        let seen = seen_turn(&mut client, 9, thread).await;
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(seen.pieces.2, 0, "translated after it was turned off");
    })
    .await;
}

/// Coding questions whose thinking is translated and scored.
const QUESTIONS: [&str; 5] = [
    "Why does this Rust code fail to compile, and how do I fix it? \
     fn main() { let s = String::from(\"a\"); let t = s; println!(\"{} {}\", s, t); }",
    "Plan how to add cursor-based pagination to GET /api/users in an Express app backed by \
     PostgreSQL. Keep it short.",
    "After `git rebase main`, src/lib.rs has conflicts and `cargo test` fails with \
     `cannot find function parse_config`. What should I check, in order?",
    "A Python service times out under load: requests pile up and memory grows. List the most \
     likely causes and how to confirm each one.",
    "Should a CLI read its config from ~/.config/tool/config.toml or from the current folder \
     first? Weigh the trade-offs briefly.",
];

/// The model's thinking for `question`.
async fn thinking_for(llm: &Llm, question: &str) -> String {
    let conversation = Conversation {
        system: None,
        messages: vec![message(
            0,
            Body::User {
                from: Speaker::Person,
                parts: vec![question.to_string()],
            },
        )],
    };
    let mut stream = llm.converse(&conversation, &[]).await.expect("start");
    let mut thinking = String::new();
    while let Some(piece) = stream.next_piece().await.expect("stream") {
        if let inocode_core::Piece::Thinking(delta) = piece {
            thinking.push_str(&delta);
        }
    }
    thinking
}

/// GEMBA-DA without a reference (Kocmi and Federmann, EAMT 2023), word for
/// word: a score from 0 to 100 for how much meaning `translation` keeps.
async fn gemba_score(llm: &Llm, source: &str, translation: &str) -> Option<f64> {
    let prompt = format!(
        "Score the following translation from English to Japanese on a continuous scale from \
         0 to 100, where a score of zero means \"no meaning preserved\" and score of one hundred \
         means \"perfect meaning and grammar\".\n\nEnglish source: \"{source}\"\nJapanese \
         translation: \"{translation}\"\nScore: "
    );
    let conversation = Conversation {
        system: None,
        messages: vec![message(
            0,
            Body::User {
                from: Speaker::Person,
                parts: vec![prompt],
            },
        )],
    };
    let (text, _) = turn(llm, &conversation, &[]).await;
    let digits: String = text
        .trim()
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    digits
        .parse()
        .ok()
        .filter(|score| (0.0..=100.0).contains(score))
}

/// Translates the thinking of real coding questions into Japanese with
/// each service in `INOCODE_TRANSLATE_SERVICES` (comma-separated; all four
/// by default) and scores every piece with GEMBA-DA by the configured
/// model. Writes every piece and score to `INOCODE_TRANSLATION_REPORT`
/// when it is set.
#[tokio::test]
#[ignore = "requires a provider credential and sends the thinking to translation services; run by hand"]
async fn thinking_translations_are_scored() {
    use inocode_core::translate::{Chunker, Hosts, Service, Translator};
    let llm = Llm::from_env().expect("provider");
    let services: Vec<Service> = std::env::var("INOCODE_TRANSLATE_SERVICES")
        .unwrap_or_else(|_| "google,edge,firefox,chromium".into())
        .split(',')
        .map(|name| Service::parse(name.trim()).expect("service"))
        .collect();
    let models = std::env::temp_dir().join("inocode-eval-models");
    let mut pieces = Vec::new();
    for question in QUESTIONS {
        let thinking = thinking_for(&llm, question).await;
        let mut chunker = Chunker::default();
        let mut cut: Vec<String> = Vec::new();
        cut.extend(chunker.push(&thinking));
        cut.extend(chunker.finish());
        // The stream cuts at sentence ends as the thinking arrives; one cut
        // of the whole may hold several sentences, so split those too.
        for chunk in cut {
            let mut sentences = Chunker::default();
            for word in chunk.split_inclusive(' ') {
                pieces.extend(sentences.push(word));
            }
            pieces.extend(sentences.finish());
        }
    }
    let pieces: Vec<String> = pieces
        .into_iter()
        .map(|piece| piece.trim().to_string())
        .filter(|piece| piece.chars().filter(|c| c.is_alphabetic()).count() >= 3)
        .collect();
    println!("pieces: {}", pieces.len());
    let mut report = Vec::new();
    for service in services {
        let translator = Translator::new(service, None, Hosts::default(), models.clone());
        let started = std::time::Instant::now();
        let translated = translator
            .translate(&pieces, "ja")
            .await
            .expect("translate");
        let seconds = started.elapsed().as_secs_f64();
        let mut scores = Vec::new();
        for (source, translation) in pieces.iter().zip(&translated) {
            let score = gemba_score(&llm, source, translation).await;
            report.push(json!({"service": service.name(), "source": source,
                "translation": translation, "score": score}));
            if let Some(score) = score {
                scores.push(score);
            }
        }
        scores.sort_by(f64::total_cmp);
        let mean = scores.iter().sum::<f64>() / scores.len().max(1) as f64;
        let below = |line: f64| scores.iter().filter(|&&score| score < line).count();
        println!(
            "RESULT service={} scored={}/{} mean={mean:.1} median={:.1} below50={} below70={} \
             translate_seconds={seconds:.2}",
            service.name(),
            scores.len(),
            pieces.len(),
            scores.get(scores.len() / 2).copied().unwrap_or(f64::NAN),
            below(50.0),
            below(70.0),
        );
    }
    if let Ok(path) = std::env::var("INOCODE_TRANSLATION_REPORT") {
        std::fs::write(path, serde_json::to_string_pretty(&report).unwrap()).expect("report");
    }
}
