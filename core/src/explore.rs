//! The explorer: a helper the model asks where things are in the project.
//! It only reads (`find`, `read`) and answers with places, file paths and
//! line ranges, so what it read does not fill the asking model's context.
//! Design and sources: docs/evidence/explore-subagent.md.

use serde::Deserialize;
use serde_json::json;
use tokio::sync::watch;

use crate::agent_tools::{self, Context};
use crate::conversation::{
    AssistantPart, Body, Conversation, Message, Speaker, StopReason, ToolSpec,
};
use crate::turn::TurnUsage;
use crate::{find, read, Llm};

pub(crate) const EXPLORE: &str = "explore";
/// Requests the explorer makes for one question at most.
const ROUNDS: usize = 20;

const INSTRUCTIONS: &str = "You find where things are in a project for another coding \
agent, which will read the places you name itself. Use find and read; you cannot change \
anything. Answer with the places only, one per line: path:first-last, then a few words on \
what is there. Copy paths, names, line numbers, numbers and error text exactly; do not \
quote code. If something was not found, say what you searched. No greetings, politeness, \
preamble or closing.";

const LAST_ROUND: &str = "Stop searching. Answer now with the places found so far, in the \
format asked for.";

pub(crate) fn spec() -> ToolSpec {
    ToolSpec {
        name: EXPLORE.into(),
        description: "Have a helper search the project and read files to find where \
            something is, without what it reads filling your context. It only reads. \
            It answers with file paths and line ranges and a few words on each, not \
            code; read the lines you need yourself. Use it when finding the place takes \
            many searches or reads across files. For one known file or one search, use \
            read or find directly."
            .into(),
        parameters: json!({
            "type": "object",
            "required": ["question"],
            "properties": {
                "question": {
                    "type": "string",
                    "description": "What to find, with the names, errors or behavior you know."
                }
            }
        }),
    }
}

#[derive(Deserialize)]
struct Arguments {
    question: String,
}

/// Has `llm` answer the question in `arguments` with the read-only tools.
pub(crate) async fn run(
    llm: &Llm,
    context: &Context<'_>,
    arguments: &str,
    usage: &mut TurnUsage,
) -> Result<String, String> {
    let Arguments { question } = serde_json::from_str(arguments)
        .map_err(|error| format!("explore needs a question: {error}"))?;
    if question.trim().is_empty() {
        return Err("explore needs a question".into());
    }
    let tools = vec![find::spec(), read::spec()];
    let mut conversation = Conversation {
        system: Some(INSTRUCTIONS.to_string()),
        messages: vec![message(Body::User {
            from: Speaker::Person,
            parts: vec![question],
        })],
    };
    let mut cancel = context.cancel.clone();
    for round in 0..=ROUNDS {
        if round == ROUNDS {
            conversation.messages.push(message(Body::User {
                from: Speaker::Harness("explore".into()),
                parts: vec![LAST_ROUND.into()],
            }));
        }
        let body = request(llm, &conversation, &tools, &mut cancel, usage).await?;
        let Body::Assistant { stop, parts, .. } = &body else {
            unreachable!("a stream assembles an assistant turn");
        };
        let calls: Vec<(String, String, String)> = parts
            .iter()
            .filter_map(|part| match part {
                AssistantPart::ToolCall {
                    call_id,
                    name,
                    arguments,
                    ..
                } => Some((call_id.clone(), name.clone(), arguments.clone())),
                _ => None,
            })
            .collect();
        if calls.is_empty() || round == ROUNDS {
            let text: String = parts
                .iter()
                .filter_map(|part| match part {
                    AssistantPart::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            let text = text.trim();
            return if text.is_empty() {
                Err("the explorer gave no answer".into())
            } else {
                Ok(text.to_string())
            };
        }
        let stop = *stop;
        conversation.messages.push(message(body));
        for (call_id, name, arguments) in calls {
            let (content, is_error) = if stop == StopReason::Length {
                (
                    "Not run: the call was cut off at the output limit.".to_string(),
                    true,
                )
            } else if name == find::FIND || name == read::READ {
                let answer = agent_tools::run(context, &name, &arguments)
                    .await
                    .unwrap_or_else(|| (format!("no tool named {name}"), true));
                // Images read here are not shown to the explorer, nor kept
                // for the asking model.
                let mut shown = context.shown.lock().expect("shown lock");
                match shown.is_empty() {
                    true => answer,
                    false => {
                        shown.clear();
                        (
                            format!("{} (the image is not shown to the explorer)", answer.0),
                            answer.1,
                        )
                    }
                }
            } else {
                (
                    format!("Not run: only find and read are available, not {name}."),
                    true,
                )
            };
            conversation.messages.push(message(Body::ToolResult {
                call_id,
                name,
                content,
                is_error,
                images: Vec::new(),
            }));
        }
    }
    unreachable!("the last round always answers")
}

/// One request: the assistant turn it produced, or why there is none.
async fn request(
    llm: &Llm,
    conversation: &Conversation,
    tools: &[ToolSpec],
    cancel: &mut watch::Receiver<bool>,
    usage: &mut TurnUsage,
) -> Result<Body, String> {
    let started = tokio::select! {
        started = llm.converse(conversation, tools) => started,
        () = crate::turn::cancelled(cancel) => return Err(STOPPED.into()),
    };
    let mut stream = started.map_err(|error| format!("the explorer failed: {}", error.message))?;
    loop {
        tokio::select! {
            next = stream.next() => match next {
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(error) => {
                    usage.add_stream(&stream, EXPLORE);
                    return Err(format!("the explorer failed: {}", error.message));
                }
            },
            () = crate::turn::cancelled(cancel) => {
                usage.add_stream(&stream, EXPLORE);
                return Err(STOPPED.into());
            }
        }
    }
    usage.add_stream(&stream, EXPLORE);
    Ok(stream.assistant())
}

const STOPPED: &str = "the explorer was stopped with the turn";

fn message(body: Body) -> Message {
    Message {
        id: String::new(),
        body,
    }
}
