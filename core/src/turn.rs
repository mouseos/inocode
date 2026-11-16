//! The turn loop the server runs for a thread: request, record, run the
//! tool calls, and decide whether to go on. Design and sources:
//! docs/evidence/turn-protocol.md.

use std::future::pending;

use serde::Serialize;
use tokio::sync::{mpsc, watch};

use crate::conversation::{AssistantPart, Body, Conversation, Message, Speaker, StopReason};
use crate::store::{SessionId, Store};
use crate::{ErrorInfo, Llm, TokenUsage};

/// What a running turn reports, in order.
#[derive(Debug, Clone, PartialEq)]
pub enum TurnEvent {
    /// Answer text as the model streams it.
    Delta(String),
    /// A message now stored in the thread.
    Message { seq: i64, message: Message },
}

/// Why a turn ended.
#[derive(Debug, Clone, PartialEq)]
pub enum Ending {
    /// The model ended its answer and no input is waiting.
    Completed,
    /// The provider cut the answer at its output limit.
    Length,
    Interrupted,
    Failed(ErrorInfo),
}

impl Ending {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Length => "length",
            Self::Interrupted => "interrupted",
            Self::Failed(_) => "failed",
        }
    }
}

/// The tokens of every request of a turn, added up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct TurnUsage {
    pub requests: u64,
    pub input: u64,
    pub output: u64,
    pub cached: u64,
}

impl TurnUsage {
    fn add(&mut self, usage: Option<TokenUsage>) {
        self.requests += 1;
        if let Some(usage) = usage {
            self.input += usage.input;
            self.output += usage.output;
            self.cached += usage.cached;
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub ending: Ending,
    pub usage: TurnUsage,
}

/// After a step, the turn goes on or ends with this.
#[derive(Debug, PartialEq)]
enum Step {
    Continue,
    Finish(Ending),
}

/// The one place that decides whether a turn goes on after an assistant
/// message. Which continuation rule is best is to be settled by experiment
/// (docs/evidence/turn-protocol.md); this is the rule Codex, DSH, and Pi share:
/// go on while the model called tools or the person sent more input.
fn next_step(stop: StopReason, steered: bool) -> Step {
    match stop {
        StopReason::ToolUse => Step::Continue,
        StopReason::Completed if steered => Step::Continue,
        StopReason::Completed => Step::Finish(Ending::Completed),
        StopReason::Length => Step::Finish(Ending::Length),
        StopReason::Error | StopReason::Aborted => Step::Finish(Ending::Interrupted),
    }
}

/// Resolves once `cancel` reads `true`. A dropped sender never cancels: the
/// turn outlives the connection that started it.
async fn cancelled(cancel: &mut watch::Receiver<bool>) {
    loop {
        if *cancel.borrow_and_update() {
            return;
        }
        if cancel.changed().await.is_err() {
            pending::<()>().await;
        }
    }
}

struct Thread<'a> {
    store: &'a Store,
    session: SessionId,
    conversation: Conversation,
    events: mpsc::Sender<TurnEvent>,
}

impl Thread<'_> {
    async fn record(&mut self, bodies: Vec<Body>) -> Result<(), ErrorInfo> {
        let base = self.conversation.messages.len();
        let messages: Vec<Message> = bodies
            .into_iter()
            .enumerate()
            .map(|(offset, body)| Message {
                id: (base + offset + 1).to_string(),
                body,
            })
            .collect();
        let positions = self.store.append(self.session, messages.clone()).await?;
        for (seq, message) in positions.into_iter().zip(messages) {
            self.conversation.messages.push(message.clone());
            // A closed connection does not stop the turn: it is stored.
            let _ = self.events.send(TurnEvent::Message { seq, message }).await;
        }
        Ok(())
    }
}

fn person(text: String) -> Body {
    Body::User {
        from: Speaker::Person,
        parts: vec![text],
    }
}

/// No tool runs yet, so every call is answered as an error; without an
/// answer the thread could not be sent again.
fn answer_calls(parts: &[AssistantPart]) -> Vec<Body> {
    parts
        .iter()
        .filter_map(|part| match part {
            AssistantPart::ToolCall { call_id, name, .. } => Some(Body::ToolResult {
                call_id: call_id.clone(),
                name: name.clone(),
                content: format!("no tool named {name} is available"),
                is_error: true,
            }),
            _ => None,
        })
        .collect()
}

/// Runs one turn of `session`, starting with `prompt`. Input from `steer`
/// joins the thread before the next request. Every message is stored as it
/// is made, so an interrupted or failed turn leaves what happened.
pub async fn run(
    llm: &Llm,
    store: &Store,
    session: SessionId,
    prompt: String,
    mut steer: mpsc::UnboundedReceiver<String>,
    mut cancel: watch::Receiver<bool>,
    events: mpsc::Sender<TurnEvent>,
) -> Outcome {
    let mut usage = TurnUsage::default();
    let ending = match load(store, session).await {
        Ok(conversation) => {
            let mut thread = Thread {
                store,
                session,
                conversation,
                events,
            };
            steps(
                llm,
                &mut thread,
                prompt,
                &mut steer,
                &mut cancel,
                &mut usage,
            )
            .await
        }
        Err(error) => Ending::Failed(error),
    };
    Outcome { ending, usage }
}

async fn load(store: &Store, session: SessionId) -> Result<Conversation, ErrorInfo> {
    let stored = store.since(session, 0, i64::MAX as usize).await?;
    Ok(Conversation {
        system: None,
        messages: stored.into_iter().map(|stored| stored.message).collect(),
    })
}

async fn steps(
    llm: &Llm,
    thread: &mut Thread<'_>,
    prompt: String,
    steer: &mut mpsc::UnboundedReceiver<String>,
    cancel: &mut watch::Receiver<bool>,
    usage: &mut TurnUsage,
) -> Ending {
    if let Err(error) = thread.record(vec![person(prompt)]).await {
        return Ending::Failed(error);
    }
    loop {
        let mut stream = tokio::select! {
            started = llm.converse(&thread.conversation, &[]) => match started {
                Ok(stream) => stream,
                Err(error) => return Ending::Failed(error),
            },
            () = cancelled(cancel) => return Ending::Interrupted,
        };
        let mut failure = None;
        loop {
            tokio::select! {
                next = stream.next() => match next {
                    Ok(Some(text)) => {
                        let _ = thread.events.send(TurnEvent::Delta(text)).await;
                    }
                    Ok(None) => break,
                    Err(error) => {
                        failure = Some(error);
                        break;
                    }
                },
                () = cancelled(cancel) => break,
            }
        }
        usage.add(stream.usage());
        let body = stream.assistant();
        let Body::Assistant { stop, parts, .. } = &body else {
            unreachable!("a stream assembles an assistant turn");
        };
        let stop = *stop;
        let mut bodies = vec![body.clone()];
        bodies.extend(answer_calls(parts));
        let mut steered = false;
        while let Ok(text) = steer.try_recv() {
            bodies.push(person(text));
            steered = true;
        }
        if let Err(error) = thread.record(bodies).await {
            return Ending::Failed(error);
        }
        if let Some(error) = failure {
            return Ending::Failed(error);
        }
        match next_step(stop, steered) {
            Step::Continue => {}
            Step::Finish(ending) => return ending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_turn_goes_on_only_after_tool_calls_or_new_input() {
        assert_eq!(next_step(StopReason::ToolUse, false), Step::Continue);
        assert_eq!(next_step(StopReason::Completed, true), Step::Continue);
        assert_eq!(
            next_step(StopReason::Completed, false),
            Step::Finish(Ending::Completed)
        );
        assert_eq!(
            next_step(StopReason::Length, true),
            Step::Finish(Ending::Length)
        );
        assert_eq!(
            next_step(StopReason::Aborted, false),
            Step::Finish(Ending::Interrupted)
        );
    }

    #[test]
    fn every_tool_call_is_answered() {
        let parts = vec![
            AssistantPart::Text { text: "x".into() },
            AssistantPart::ToolCall {
                call_id: "c1".into(),
                name: "read".into(),
                arguments: "{}".into(),
                replay: None,
            },
        ];
        assert_eq!(
            answer_calls(&parts),
            vec![Body::ToolResult {
                call_id: "c1".into(),
                name: "read".into(),
                content: "no tool named read is available".into(),
                is_error: true,
            }]
        );
    }
}
