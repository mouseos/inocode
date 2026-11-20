//! The turn loop the server runs for a thread: request, record, run the
//! tool calls, and decide whether to go on. Design and sources:
//! docs/evidence/turn-protocol.md and docs/evidence/agent-tools.md.

use std::collections::{HashMap, HashSet};
use std::future::pending;
use std::path::{Path, PathBuf};

use serde::Serialize;
use tokio::sync::{mpsc, watch};

use crate::agent_tools::{self, Answer};
use crate::compaction;
use crate::conversation::{
    AssistantPart, Body, Conversation, Message, Speaker, StopReason, ToolSpec,
};
use crate::rules::{Actor, Decision};
use crate::shell;
use crate::store::{NoteId, ProjectId, SessionId, Store};
use crate::{ErrorInfo, Llm, TokenUsage};

/// The project rules file read from the project's directory.
const RULES_FILE: &str = "AGENTS.md";
/// Bytes of it given to the model (Codex `DEFAULT_PROJECT_DOC_MAX_BYTES`).
const RULES_MAX_BYTES: usize = 32 * 1024;
/// Identical failures of one call after which its result carries a warning.
const REPEAT_WARNING: u32 = 3;
/// Answers cut at the output limit in a row that are each followed by a
/// request to go on; the next cut ends the turn (Claude Code's count,
/// docs/evidence/output-limit.md).
const RESUMES: u32 = 3;
/// Speaker kind of that request.
const RESUME_KIND: &str = "resume";
const RESUME: &str = "Your previous reply was cut off at the output limit. Continue exactly \
where it stopped, without apologizing, repeating or summarizing what you already wrote. If \
what remains is long, split it into smaller pieces.";

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

/// A call that waits for the person's approval. It was not run; approving
/// its `ask` rules, or the call itself, on the next turn lets it run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Held {
    pub rules: Vec<i64>,
    pub titles: Vec<String>,
    pub tool: String,
    pub arguments: String,
    /// Why it needs the person.
    pub reason: String,
}

/// How much a turn may do without the person. Design:
/// docs/evidence/approval-modes.md.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// Every change waits for the person.
    Ask,
    /// Commands run in the sandbox; the model asks when it needs more.
    #[default]
    OnRequest,
    /// Commands run in the sandbox; a reviewer decides in the person's place.
    Auto,
    /// No sandbox and no approvals.
    Full,
}

impl Mode {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "ask" => Some(Self::Ask),
            "on-request" => Some(Self::OnRequest),
            "auto" => Some(Self::Auto),
            "full" => Some(Self::Full),
            _ => None,
        }
    }
}

/// What the person allowed for one turn: its mode, the `ask` rules
/// approved, and held calls approved by their exact tool and arguments,
/// each for one run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Approvals {
    pub mode: Mode,
    pub rules: Vec<NoteId>,
    pub calls: Vec<(String, String)>,
}

/// Refusals of the reviewer in a row, and in a turn, after which calls go
/// to the person instead (the counts of Claude Code and DeepAI Code).
const DENIALS_IN_A_ROW: u32 = 3;
const DENIALS_PER_TURN: u32 = 20;
/// Names a model to review calls in the `auto` mode instead of the turn's.
const REVIEW_MODEL_ENV: &str = "INOCODE_REVIEW_MODEL";
const COMPACT_MODEL_ENV: &str = "INOCODE_COMPACT_MODEL";

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
    /// Go on after asking the model to continue its cut answer.
    Resume,
    Finish(Ending),
}

/// The one place that decides whether a turn goes on after an assistant
/// message. Which continuation rule is best is to be settled by experiment
/// (docs/evidence/turn-protocol.md); this is the rule Codex, DSH, and Pi share:
/// go on while the model called tools or the person sent more input. An
/// answer cut at the output limit is resumed while `cuts`, the cut answers
/// just before it, is under `RESUMES`.
fn next_step(stop: StopReason, steered: bool, cuts: u32) -> Step {
    match stop {
        StopReason::ToolUse => Step::Continue,
        StopReason::Completed if steered => Step::Continue,
        StopReason::Completed => Step::Finish(Ending::Completed),
        StopReason::Length if cuts < RESUMES => Step::Resume,
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
    let mut system = String::from(
        "You are a coding agent working on a user's project. When a method is shown to \
         work, save it with the method tool. Before investigating a device, format or \
         program from scratch, use recall to look for methods saved in earlier work.",
    );
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
    /// The project folder, where commands run.
    folder: Option<PathBuf>,
    rules: Option<String>,
    cancel: watch::Receiver<bool>,
    /// What is sent, and the stored position of each message in it.
    conversation: Conversation,
    seqs: Vec<i64>,
    /// The position the next stored message takes.
    next_seq: i64,
    /// The position of this turn's prompt; the person's messages from here
    /// on are always sent whole.
    turn_start: i64,
    events: mpsc::Sender<TurnEvent>,
    /// `ask` rules the person approved for this turn.
    allowed: HashSet<NoteId>,
    held: Vec<Held>,
    /// Identical failed calls and how often they failed.
    failures: HashMap<(String, String, String), u32>,
    /// Lessons that may apply to this turn's request.
    lessons: Option<String>,
    mode: Mode,
    /// Held calls the person approved, each run once.
    approved: Vec<(String, String)>,
    /// The model that reviews calls when it is not the turn's own.
    reviewer: Option<Llm>,
    /// The model that writes compaction summaries when it is not the turn's own.
    compactor: Option<std::sync::Arc<Llm>>,
    /// The reviewer's refusals in a row and in this turn.
    denials: (u32, u32),
}

impl Thread<'_> {
    async fn record(&mut self, bodies: Vec<Body>) -> Result<(), ErrorInfo> {
        let base = self.next_seq;
        let messages: Vec<Message> = bodies
            .into_iter()
            .enumerate()
            .map(|(offset, body)| Message {
                id: (base + offset as i64).to_string(),
                body,
            })
            .collect();
        let positions = self.store.append(self.session, messages.clone()).await?;
        for (seq, message) in positions.into_iter().zip(messages) {
            self.conversation.messages.push(message.clone());
            self.seqs.push(seq);
            self.next_seq = seq + 1;
            // A closed connection does not stop the turn: it is stored.
            let _ = self.events.send(TurnEvent::Message { seq, message }).await;
        }
        Ok(())
    }

    /// Compacts what is about to be sent when it would fill most of the
    /// context window. Its size, with the plan and lessons sent after it, is
    /// estimated from its bytes at the bytes per token this model measured
    /// on the thread's latest request, so the messages added since that
    /// request count too. Before the thread has a measured request with
    /// this model (another model, a provider that reports no usage) the
    /// rate runs high, so then only a size over the whole window compacts.
    /// The stored history is kept; a compaction message is added and the
    /// history is assembled again from the store.
    async fn compact_if_due(
        &mut self,
        llm: &Llm,
        tools: &[ToolSpec],
        usage: &mut TurnUsage,
    ) -> Result<(), ErrorInfo> {
        let Some(window) = llm.context_window().await else {
            return Ok(());
        };
        let state = self.state().await?;
        let bytes = compaction::request_bytes(
            self.conversation.system.as_deref(),
            &self.conversation.messages,
            tools,
        ) + state.map_or(0, |state| state.len() as u64);
        let (input, due) = match llm.bytes_per_token(self.session.0) {
            Some(ratio) => {
                let input = compaction::tokens(bytes, ratio);
                (input, compaction::due(input, window))
            }
            None => {
                let input = compaction::tokens(bytes, compaction::FALLBACK_BYTES_PER_TOKEN);
                (input, input >= window)
            }
        };
        if !due {
            return Ok(());
        }
        let sent: Vec<(i64, Message)> = self
            .seqs
            .iter()
            .copied()
            .zip(self.conversation.messages.iter().cloned())
            .collect();
        let compacted = compaction::compact(
            llm,
            self.compactor.as_deref(),
            self.conversation.system.clone(),
            &sent,
            tools,
            input,
            window,
        )
        .await;
        let Some((body, summary_usages)) = compacted else {
            return Ok(());
        };
        for summary_usage in summary_usages {
            usage.add(Some(summary_usage));
        }
        self.record(vec![body]).await?;
        let stored = self.store.since(self.session, 0, i64::MAX as usize).await?;
        let (seqs, messages) = compaction::assemble(stored).into_iter().unzip();
        self.seqs = seqs;
        self.conversation.messages = messages;
        self.shorten_people(window);
        Ok(())
    }

    /// Sends the person's large messages from earlier turns shortened, each
    /// saved whole once under the store's files folder
    /// ([`compaction::shorten_people`]).
    fn shorten_people(&mut self, window: u64) {
        let folder = self.store.files_folder().join("messages");
        let session = self.session.0;
        compaction::shorten_people(
            &mut self.conversation.messages,
            &self.seqs,
            self.turn_start,
            window,
            |seq, text| {
                let path = folder.join(format!("{session}-{seq}.txt"));
                if !path.exists() {
                    std::fs::create_dir_all(&folder).ok()?;
                    std::fs::write(&path, text).ok()?;
                }
                Some(path)
            },
        );
    }

    /// The plan and the lessons, shown after the history with each request
    /// and never stored, so they survive compaction and keep the cached
    /// prefix intact.
    async fn state(&self) -> Result<Option<String>, ErrorInfo> {
        let plan = agent_tools::render_plan(self.store, self.session).await?;
        let jobs = shell::render_jobs(&self.store.files_folder().join("logs"), self.session);
        let parts: Vec<&str> = [plan.as_deref(), jobs.as_deref(), self.lessons.as_deref()]
            .into_iter()
            .flatten()
            .collect();
        Ok((!parts.is_empty()).then(|| parts.join("\n")))
    }

    /// Whether this turn's commands run confined.
    fn sandboxed(&self) -> bool {
        self.mode != Mode::Full && crate::sandbox::enabled()
    }

    /// Answers one call of `pending`, the assistant message being answered:
    /// checks it against the project's must rules and the turn's mode, has
    /// it reviewed in the `auto` mode, then runs it. A rule check that fails
    /// keeps the call from running.
    async fn answer(
        &mut self,
        llm: &Llm,
        pending: &Body,
        name: &str,
        arguments: &str,
        usage: &mut TurnUsage,
    ) -> Answer {
        let decision = match self.store.check(self.project, name, arguments, None).await {
            Ok(decision) => decision,
            Err(error) => {
                return (
                    format!("Not run: the rule check failed ({}).", error.message),
                    true,
                )
            }
        };
        let mut asked_rules = Vec::new();
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
                if !rules.iter().all(|rule| self.allowed.contains(rule)) {
                    asked_rules = rules;
                } else {
                    for rule in &rules {
                        let _ = self
                            .store
                            .record(*rule, "allowed", Actor::Person, None, name)
                            .await;
                    }
                }
            }
        }
        let approved = self
            .approved
            .iter()
            .position(|(tool, args)| tool == name && args == arguments);
        if let Some(index) = approved {
            self.approved.remove(index);
            for rule in &asked_rules {
                let _ = self
                    .store
                    .record(*rule, "allowed", Actor::Person, None, name)
                    .await;
            }
        } else if self.mode != Mode::Full {
            if let Some(answer) = self
                .approval(llm, pending, name, arguments, asked_rules, usage)
                .await
            {
                return answer;
            }
        }
        let context = agent_tools::Context {
            store: self.store,
            session: self.session,
            project: self.project,
            rules: self.rules.as_deref(),
            folder: self.folder.as_deref(),
            cancel: self.cancel.clone(),
            sandboxed: self.sandboxed(),
        };
        agent_tools::run(&context, name, arguments)
            .await
            .unwrap_or_else(|| (format!("no tool named {name} is available"), true))
    }

    /// Why a call needs approval in this mode, if it does; `asked_rules`
    /// are the `ask` rules it met that the person has not approved.
    fn needs(&self, name: &str, arguments: &str, asked_rules: bool) -> Option<String> {
        let command = (name == shell::SHELL)
            .then(|| shell::shell_arguments(arguments))
            .flatten();
        if asked_rules {
            return Some("it falls under a project rule that asks first".into());
        }
        if self.sandboxed() && command.as_ref().is_some_and(|c| c.outside_sandbox) {
            return Some("it asks to run outside the sandbox".into());
        }
        let reads = command
            .as_ref()
            .is_some_and(|c| shell::reads_only(&c.command));
        let changes = matches!(name, crate::edit::EDIT | crate::edit::WRITE)
            || (name == shell::SHELL && !reads);
        match self.mode {
            Mode::Ask if changes => Some("every change waits for the person in this mode".into()),
            Mode::Auto if name == shell::SHELL && !reads => {
                Some("commands that change something are reviewed in this mode".into())
            }
            _ => None,
        }
    }

    /// Holds the call for the person, or in the `auto` mode has it reviewed.
    /// `None` lets the call run.
    async fn approval(
        &mut self,
        llm: &Llm,
        pending: &Body,
        name: &str,
        arguments: &str,
        asked_rules: Vec<NoteId>,
        usage: &mut TurnUsage,
    ) -> Option<Answer> {
        let reason = self.needs(name, arguments, !asked_rules.is_empty())?;
        if self.mode == Mode::Auto {
            if self.denials.0 >= DENIALS_IN_A_ROW || self.denials.1 >= DENIALS_PER_TURN {
                let reason = "the reviewer refused too many calls in this turn".to_string();
                return Some(self.hold(name, arguments, asked_rules, reason).await);
            }
            let mut seen = self.conversation.messages.clone();
            seen.push(Message {
                id: "pending".to_string(),
                body: pending.clone(),
            });
            let seen = Conversation {
                system: None,
                messages: seen,
            };
            let (verdict, spent) = crate::review::review(
                llm,
                self.reviewer.as_ref().unwrap_or(llm),
                &seen,
                self.folder.as_deref(),
                self.rules.as_deref(),
                name,
                arguments,
            )
            .await;
            if let Some(spent) = spent {
                usage.add(spent);
            }
            return match verdict {
                crate::review::Verdict::Allow => {
                    self.denials.0 = 0;
                    for rule in &asked_rules {
                        let _ = self
                            .store
                            .record(*rule, "allowed", Actor::Harness, None, name)
                            .await;
                    }
                    None
                }
                crate::review::Verdict::Deny(why) => {
                    self.denials.0 += 1;
                    self.denials.1 += 1;
                    Some((
                        format!(
                            "Denied by the reviewer: {why}. It was not run. Do not retry it or \
                             work around the refusal; go on with other work, or if this exact \
                             action is essential, say so to the person."
                        ),
                        true,
                    ))
                }
                crate::review::Verdict::Unavailable(why) => {
                    let reason = format!("the reviewer could not decide ({why})");
                    Some(self.hold(name, arguments, asked_rules, reason).await)
                }
            };
        }
        Some(self.hold(name, arguments, asked_rules, reason).await)
    }

    async fn hold(
        &mut self,
        name: &str,
        arguments: &str,
        rules: Vec<NoteId>,
        reason: String,
    ) -> Answer {
        let titles = self.titles(&rules).await;
        let under = if titles.is_empty() {
            String::new()
        } else {
            format!(" (project rule: {})", titles.join("; "))
        };
        let text = format!(
            "Held for the person's approval: {reason}{under}. It was not run. Go on with \
             work that does not depend on it; the person sees the held calls when this \
             turn ends."
        );
        self.held.push(Held {
            rules: rules.iter().map(|rule| rule.0).collect(),
            titles,
            tool: name.to_string(),
            arguments: arguments.to_string(),
            reason,
        });
        (text, true)
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
    async fn results(&mut self, llm: &Llm, body: &Body, usage: &mut TurnUsage) -> Vec<Body> {
        let Body::Assistant { stop, parts, .. } = body else {
            return Vec::new();
        };
        let stop = *stop;
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
                    let answer = self.answer(llm, body, name, arguments, usage).await;
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

/// Drops from an answer cut at the output limit the tool call whose
/// arguments were cut mid-way: they are not JSON, so no provider would take
/// the conversation back with them in it.
fn drop_cut_calls(body: &mut Body) {
    if let Body::Assistant {
        stop: StopReason::Length,
        parts,
        ..
    } = body
    {
        parts.retain(|part| match part {
            AssistantPart::ToolCall { arguments, .. } => {
                arguments.trim().is_empty()
                    || serde_json::from_str::<serde_json::Value>(arguments)
                        .is_ok_and(|value| value.is_object())
            }
            _ => true,
        });
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
/// is made, so an interrupted or failed turn leaves what happened. The turn
/// works in the mode of `approvals`, and what it lists runs as approved by
/// the person.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    llm: &Llm,
    store: &Store,
    session: SessionId,
    prompt: String,
    approvals: Approvals,
    mut steer: mpsc::UnboundedReceiver<String>,
    mut cancel: watch::Receiver<bool>,
    events: mpsc::Sender<TurnEvent>,
) -> Outcome {
    let mut usage = TurnUsage::default();
    let reviewer = match reviewer(llm, approvals.mode)
        .and_then(|reviewer| compactor(llm).map(|compactor| (reviewer, compactor)))
    {
        Ok(models) => models,
        Err(error) => {
            return Outcome {
                ending: Ending::Failed(error),
                usage,
                held: Vec::new(),
            }
        }
    };
    let mut thread = match open(store, session, &prompt, approvals, cancel.clone(), events).await {
        Ok(mut thread) => {
            (thread.reviewer, thread.compactor) = reviewer;
            if let Some(window) = llm.context_window().await {
                thread.shorten_people(window);
            }
            thread
        }
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
    if ending == Ending::Interrupted {
        shell::stop_all(&store.files_folder().join("logs"), session);
    }
    Outcome {
        ending,
        usage,
        held: thread.held,
    }
}

/// The model that reviews calls in the `auto` mode when it is not the
/// turn's own: the one `INOCODE_REVIEW_MODEL` names, on the same provider.
fn reviewer(llm: &Llm, mode: Mode) -> Result<Option<Llm>, ErrorInfo> {
    if mode != Mode::Auto {
        return Ok(None);
    }
    match std::env::var(REVIEW_MODEL_ENV) {
        Ok(model) if !model.trim().is_empty() => llm.with_model(model.trim()).map(Some),
        _ => Ok(None),
    }
}

/// The model that writes compaction summaries when it is not the turn's
/// own: the one `INOCODE_COMPACT_MODEL` names, on the same provider.
fn compactor(llm: &Llm) -> Result<Option<std::sync::Arc<Llm>>, ErrorInfo> {
    match std::env::var(COMPACT_MODEL_ENV) {
        Ok(model) if !model.trim().is_empty() && model.trim() != llm.model() => {
            llm.sibling(model.trim()).map(Some)
        }
        _ => Ok(None),
    }
}

async fn open<'a>(
    store: &'a Store,
    session: SessionId,
    prompt: &str,
    approvals: Approvals,
    cancel: watch::Receiver<bool>,
    events: mpsc::Sender<TurnEvent>,
) -> Result<Thread<'a>, ErrorInfo> {
    let project = store.session_project(session).await?;
    let rules = match &project {
        Some((_, key)) => read_rules(key).await,
        None => None,
    };
    let folder = project.as_ref().map(|(_, key)| PathBuf::from(key));
    let project = project.map(|(id, _)| id);
    let stored = store.since(session, 0, i64::MAX as usize).await?;
    let next_seq = stored.last().map_or(1, |last| last.seq + 1);
    let (seqs, messages) = compaction::assemble(stored).into_iter().unzip();
    let lessons = agent_tools::render_lessons(store, project, prompt).await?;
    Ok(Thread {
        store,
        session,
        project,
        folder,
        cancel,
        conversation: Conversation {
            system: Some(system_prompt(rules.as_deref())),
            messages,
        },
        seqs,
        next_seq,
        turn_start: next_seq,
        rules,
        events,
        allowed: approvals.rules.into_iter().collect(),
        held: Vec::new(),
        failures: HashMap::new(),
        lessons,
        mode: approvals.mode,
        approved: approvals.calls,
        reviewer: None,
        compactor: None,
        denials: (0, 0),
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
    let tools: Vec<ToolSpec> = agent_tools::specs(thread.sandboxed());
    if let Err(error) = thread.record(vec![person(prompt)]).await {
        return Ending::Failed(error);
    }
    let mut cuts = 0;
    loop {
        let compacted = tokio::select! {
            compacted = thread.compact_if_due(llm, &tools, usage) => Some(compacted),
            () = cancelled(cancel) => None,
        };
        match compacted {
            Some(Ok(())) => {}
            Some(Err(error)) => return Ending::Failed(error),
            None => return Ending::Interrupted,
        }
        let state = match thread.state().await {
            Ok(state) => state,
            Err(error) => return Ending::Failed(error),
        };
        let mut sent = Conversation {
            system: thread.conversation.system.clone(),
            messages: thread.conversation.messages.clone(),
        };
        if let Some(state) = state {
            sent.messages.push(Message {
                id: "state".to_string(),
                body: Body::User {
                    from: Speaker::Harness(crate::conversation::STATE.to_string()),
                    parts: vec![state],
                },
            });
        }
        let started = tokio::select! {
            started = llm.converse(&sent, &tools) => Some(started),
            () = cancelled(cancel) => None,
        };
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
        if let Some(measured) = stream.usage() {
            llm.measured(
                thread.session.0,
                compaction::request_bytes(sent.system.as_deref(), &sent.messages, &tools),
                measured.input,
            );
        }
        let mut body = stream.assistant();
        drop_cut_calls(&mut body);
        let Body::Assistant { stop, .. } = &body else {
            unreachable!("a stream assembles an assistant turn");
        };
        let stop = *stop;
        let results = thread.results(llm, &body, usage).await;
        let mut bodies = vec![body.clone()];
        bodies.extend(results);
        let mut steered = false;
        while let Ok(text) = steer.try_recv() {
            bodies.push(person(text));
            steered = true;
        }
        let step = match failure {
            Some(_) => None,
            None => Some(next_step(stop, steered, cuts)),
        };
        if step == Some(Step::Resume) {
            bodies.push(Body::User {
                from: Speaker::Harness(RESUME_KIND.to_string()),
                parts: vec![RESUME.to_string()],
            });
        }
        if let Err(error) = thread.record(bodies).await {
            return Ending::Failed(error);
        }
        match step {
            None => return Ending::Failed(failure.expect("no step only after a failure")),
            Some(Step::Continue) => cuts = 0,
            Some(Step::Resume) => cuts += 1,
            Some(Step::Finish(ending)) => return ending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_turn_goes_on_only_after_tool_calls_or_new_input() {
        assert_eq!(next_step(StopReason::ToolUse, false, 0), Step::Continue);
        assert_eq!(next_step(StopReason::Completed, true, 0), Step::Continue);
        assert_eq!(
            next_step(StopReason::Completed, false, 0),
            Step::Finish(Ending::Completed)
        );
        assert_eq!(
            next_step(StopReason::Aborted, false, 0),
            Step::Finish(Ending::Interrupted)
        );
    }

    #[test]
    fn a_cut_answer_is_resumed_three_times_in_a_row_then_ends_the_turn() {
        for cuts in 0..RESUMES {
            assert_eq!(next_step(StopReason::Length, false, cuts), Step::Resume);
        }
        assert_eq!(
            next_step(StopReason::Length, true, RESUMES),
            Step::Finish(Ending::Length)
        );
    }

    #[test]
    fn the_rules_are_given_whole_inside_markers() {
        let system = system_prompt(Some("- テストは全部実行する"));
        assert!(
            system.ends_with("----- RULES -----\n- テストは全部実行する\n----- END RULES -----")
        );
        let bare = system_prompt(None);
        assert!(bare.starts_with("You are a coding agent working on a user's project."));
        assert!(!bare.contains("RULES"));
    }
}
