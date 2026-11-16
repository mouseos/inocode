//! The turn loop the server runs for a thread: request, record, run the
//! tool calls, and decide whether to go on. Design and sources:
//! docs/evidence/turn-protocol.md and docs/evidence/agent-tools.md.

use std::collections::{HashMap, HashSet};
use std::future::pending;
use std::path::Path;

use serde::Serialize;
use tokio::sync::{mpsc, watch};

use crate::agent_tools::{self, Answer};
use crate::conversation::{
    AssistantPart, Body, Conversation, Message, Speaker, StopReason, ToolSpec,
};
use crate::rules::{Actor, Decision};
use crate::store::{NoteId, ProjectId, SessionId, Store};
use crate::{ErrorInfo, Llm, TokenUsage};

/// The project rules file read from the project's directory.
const RULES_FILE: &str = "AGENTS.md";
/// Bytes of it given to the model (Codex `DEFAULT_PROJECT_DOC_MAX_BYTES`).
const RULES_MAX_BYTES: usize = 32 * 1024;
/// Identical failures of one call after which its result carries a warning.
const REPEAT_WARNING: u32 = 3;

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

/// A call that waits for the person's approval under `ask` rules. It was
/// not run; approving the rules on the next turn lets it run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Held {
    pub rules: Vec<i64>,
    pub titles: Vec<String>,
    pub tool: String,
    pub arguments: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub ending: Ending,
    pub usage: TurnUsage,
    pub held: Vec<Held>,
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

/// Reads the project's rules file; `None` when the project has none.
/// Longer files are cut at the byte limit, as Codex does.
async fn read_rules(project_key: &str) -> Option<String> {
    let text = tokio::fs::read_to_string(Path::new(project_key).join(RULES_FILE))
        .await
        .ok()?;
    let mut end = text.len().min(RULES_MAX_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let text = text[..end].trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// The system prompt: the project's rules as written, never summarized.
fn system_prompt(rules: Option<&str>) -> String {
    let mut system = String::from("You are a coding agent working on a user's project.");
    if let Some(rules) = rules {
        system.push_str(" Follow the project's rules below.\n----- RULES -----\n");
        system.push_str(rules);
        system.push_str("\n----- END RULES -----");
    }
    system
}

struct Thread<'a> {
    store: &'a Store,
    session: SessionId,
    project: Option<ProjectId>,
    rules: Option<String>,
    conversation: Conversation,
    events: mpsc::Sender<TurnEvent>,
    /// `ask` rules the person approved for this turn.
    allowed: HashSet<NoteId>,
    held: Vec<Held>,
    /// Identical failed calls and how often they failed.
    failures: HashMap<(String, String, String), u32>,
    /// Lessons that may apply to this turn's request.
    lessons: Option<String>,
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

    /// The plan and the lessons, shown after the history with each request
    /// and never stored, so they survive compaction and keep the cached
    /// prefix intact.
    async fn state(&self) -> Result<Option<String>, ErrorInfo> {
        let plan = agent_tools::render_plan(self.store, self.session).await?;
        let parts: Vec<&str> = [plan.as_deref(), self.lessons.as_deref()]
            .into_iter()
            .flatten()
            .collect();
        Ok((!parts.is_empty()).then(|| parts.join("\n")))
    }

    /// Answers one call: checks it against the project's must rules, then
    /// runs it. A rule check that fails keeps the call from running.
    async fn answer(&mut self, name: &str, arguments: &str) -> Answer {
        let decision = match self.store.check(self.project, name, arguments, None).await {
            Ok(decision) => decision,
            Err(error) => {
                return (
                    format!("Not run: the rule check failed ({}).", error.message),
                    true,
                )
            }
        };
        match decision {
            Decision::Allow => {}
            Decision::Block(rules) => {
                let titles = self.titles(&rules).await;
                return (
                    format!(
                        "Blocked by the project rule: {}. Do not retry this call or work \
                         around the rule; if the task needs it, say so to the person.",
                        titles.join("; ")
                    ),
                    true,
                );
            }
            Decision::Ask(rules) => {
                if rules.iter().all(|rule| self.allowed.contains(rule)) {
                    for rule in &rules {
                        let _ = self
                            .store
                            .record(*rule, "allowed", Actor::Person, None, name)
                            .await;
                    }
                } else {
                    let titles = self.titles(&rules).await;
                    let text = format!(
                        "Held for the person's approval under the project rule: {}. It was \
                         not run. Go on with work that does not depend on it; the person \
                         sees the held calls when this turn ends.",
                        titles.join("; ")
                    );
                    self.held.push(Held {
                        rules: rules.iter().map(|rule| rule.0).collect(),
                        titles,
                        tool: name.to_string(),
                        arguments: arguments.to_string(),
                    });
                    return (text, true);
                }
            }
        }
        let context = agent_tools::Context {
            store: self.store,
            session: self.session,
            project: self.project,
            rules: self.rules.as_deref(),
        };
        agent_tools::run(&context, name, arguments)
            .await
            .unwrap_or_else(|| (format!("no tool named {name} is available"), true))
    }

    async fn titles(&self, rules: &[NoteId]) -> Vec<String> {
        let mut titles = Vec::with_capacity(rules.len());
        for rule in rules {
            let title = match self.store.note(*rule).await {
                Ok(Some(note)) => note.title,
                _ => format!("rule {}", rule.0),
            };
            titles.push(title);
        }
        titles
    }

    /// Adds a warning to a failure this exact call has met before.
    fn note_repeats(&mut self, name: &str, arguments: &str, answer: Answer) -> Answer {
        let (content, is_error) = answer;
        if !is_error {
            return (content, false);
        }
        let count = self
            .failures
            .entry((name.to_string(), arguments.to_string(), content.clone()))
            .or_insert(0);
        *count += 1;
        if *count < REPEAT_WARNING {
            return (content, true);
        }
        let warning = format!(
            "\n\nThis exact call has now failed the same way {count} times. Do not repeat \
             it: change the approach, or report what blocks you."
        );
        (content + &warning, true)
    }

    /// Results for the calls of an assistant message. Calls run only when
    /// the model stopped to have them run; the calls of an answer cut at
    /// its length limit may be incomplete and are refused.
    async fn results(&mut self, stop: StopReason, parts: &[AssistantPart]) -> Vec<Body> {
        let mut results = Vec::new();
        for part in parts {
            let AssistantPart::ToolCall {
                call_id,
                name,
                arguments,
                ..
            } = part
            else {
                continue;
            };
            let answer = match stop {
                StopReason::ToolUse | StopReason::Completed => {
                    let answer = self.answer(name, arguments).await;
                    self.note_repeats(name, arguments, answer)
                }
                StopReason::Length => (
                    "Not run: the answer was cut off at the output limit before this call \
                     was complete."
                        .to_string(),
                    true,
                ),
                StopReason::Error | StopReason::Aborted => continue,
            };
            results.push(Body::ToolResult {
                call_id: call_id.clone(),
                name: name.clone(),
                content: answer.0,
                is_error: answer.1,
            });
        }
        results
    }
}

fn person(text: String) -> Body {
    Body::User {
        from: Speaker::Person,
        parts: vec![text],
    }
}

/// Runs one turn of `session`, starting with `prompt`. Input from `steer`
/// joins the thread before the next request. Every message is stored as it
/// is made, so an interrupted or failed turn leaves what happened. Calls
/// under the `ask` rules in `allowed` run as approved by the person.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    llm: &Llm,
    store: &Store,
    session: SessionId,
    prompt: String,
    allowed: Vec<NoteId>,
    mut steer: mpsc::UnboundedReceiver<String>,
    mut cancel: watch::Receiver<bool>,
    events: mpsc::Sender<TurnEvent>,
) -> Outcome {
    let mut usage = TurnUsage::default();
    let mut thread = match open(store, session, &prompt, allowed, events).await {
        Ok(thread) => thread,
        Err(error) => {
            return Outcome {
                ending: Ending::Failed(error),
                usage,
                held: Vec::new(),
            }
        }
    };
    let ending = steps(
        llm,
        &mut thread,
        prompt,
        &mut steer,
        &mut cancel,
        &mut usage,
    )
    .await;
    Outcome {
        ending,
        usage,
        held: thread.held,
    }
}

async fn open<'a>(
    store: &'a Store,
    session: SessionId,
    prompt: &str,
    allowed: Vec<NoteId>,
    events: mpsc::Sender<TurnEvent>,
) -> Result<Thread<'a>, ErrorInfo> {
    let project = store.session_project(session).await?;
    let rules = match &project {
        Some((_, key)) => read_rules(key).await,
        None => None,
    };
    let project = project.map(|(id, _)| id);
    let stored = store.since(session, 0, i64::MAX as usize).await?;
    let lessons = agent_tools::render_lessons(store, project, prompt).await?;
    Ok(Thread {
        store,
        session,
        project,
        conversation: Conversation {
            system: Some(system_prompt(rules.as_deref())),
            messages: stored.into_iter().map(|stored| stored.message).collect(),
        },
        rules,
        events,
        allowed: allowed.into_iter().collect(),
        held: Vec::new(),
        failures: HashMap::new(),
        lessons,
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
    let tools: Vec<ToolSpec> = agent_tools::specs();
    if let Err(error) = thread.record(vec![person(prompt)]).await {
        return Ending::Failed(error);
    }
    loop {
        let state = match thread.state().await {
            Ok(state) => state,
            Err(error) => return Ending::Failed(error),
        };
        if let Some(state) = &state {
            thread.conversation.messages.push(Message {
                id: "state".to_string(),
                body: Body::User {
                    from: Speaker::Harness("state".to_string()),
                    parts: vec![state.clone()],
                },
            });
        }
        let started = tokio::select! {
            started = llm.converse(&thread.conversation, &tools) => Some(started),
            () = cancelled(cancel) => None,
        };
        if state.is_some() {
            thread.conversation.messages.pop();
        }
        let mut stream = match started {
            Some(Ok(stream)) => stream,
            Some(Err(error)) => return Ending::Failed(error),
            None => return Ending::Interrupted,
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
        let results = thread.results(stop, parts).await;
        let mut bodies = vec![body.clone()];
        bodies.extend(results);
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
    fn the_rules_are_given_whole_inside_markers() {
        let system = system_prompt(Some("- テストは全部実行する"));
        assert!(
            system.ends_with("----- RULES -----\n- テストは全部実行する\n----- END RULES -----")
        );
        assert_eq!(
            system_prompt(None),
            "You are a coding agent working on a user's project."
        );
    }
}
