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
    /// A question of an MCP server for the person, answered with
    /// `turn/answer` while the call waits.
    Elicitation(serde_json::Value),
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

/// The tokens of every request of a turn, added up, and the requests as
/// they are logged.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct TurnUsage {
    pub requests: u64,
    pub input: u64,
    pub output: u64,
    pub cached: u64,
    #[serde(skip)]
    pub records: Vec<crate::RequestRecord>,
}

impl TurnUsage {
    /// Adds a streamed request, and logs it as made for `purpose`.
    pub(crate) fn add_stream(&mut self, stream: &crate::ChatStream, purpose: &'static str) {
        self.add(stream.usage());
        self.records.push(stream.record(purpose));
    }

    pub(crate) fn add(&mut self, usage: Option<TokenUsage>) {
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
const EXPLORE_MODEL_ENV: &str = "INOCODE_EXPLORE_MODEL";
/// Names a model to check goals instead of the turn's
/// (docs/evidence/goal-command.md).
const CHECK_MODEL_ENV: &str = "INOCODE_CHECK_MODEL";

#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub ending: Ending,
    pub usage: TurnUsage,
    pub held: Vec<Held>,
    pub questions: Vec<Asked>,
}

/// Questions the model asked the person with `ask`, as the form they make.
/// `seq` is the stored answer of the call, which a reply to the questions
/// names; `None` when it is not in the thread's history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Asked {
    pub seq: Option<i64>,
    pub questions: serde_json::Value,
}

/// What the person sent to start a turn: the text, and the earlier message
/// of the thread it replies to, if any (its seq).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prompt {
    pub text: String,
    pub reply_to: Option<i64>,
}

/// Characters of a replied-to message that are quoted; the rest is read
/// with `recall`.
const QUOTE_CHARS: usize = 2_000;
const REPLY_KIND: &str = "reply";
const ESCALATION_KIND: &str = "escalation";
/// Characters of a turn's last words kept with an unfinished attempt.
const ATTEMPT_CHARS: usize = 1_000;

/// The replied-to message `seq` of `session`, quoted for the model, so the
/// next message can be read as a reply to it (docs/evidence/reply-to-message.md).
async fn quote(store: &Store, session: SessionId, seq: i64) -> Result<Body, ErrorInfo> {
    let stored = store
        .since(session, seq - 1, 1)
        .await?
        .into_iter()
        .next()
        .filter(|stored| stored.seq == seq)
        .ok_or_else(|| {
            ErrorInfo::new(
                "invalid_params",
                format!("the thread has no message {seq} to reply to"),
            )
        })?;
    // Answers to questions put to the person may hold for later work too.
    let question = match &stored.message.body {
        Body::ToolResult { name, .. } => name == crate::ask::ASK,
        Body::User {
            from: Speaker::Harness(kind),
            ..
        } => kind == ESCALATION_KIND,
        _ => false,
    };
    let (author, text) = match stored.message.body {
        Body::User {
            from: Speaker::Person,
            parts,
        } => ("the person".to_string(), parts.join("\n")),
        Body::User { parts, .. } => ("the harness".to_string(), parts.join("\n")),
        Body::Assistant { parts, .. } => (
            "you".to_string(),
            parts
                .into_iter()
                .filter_map(|part| match part {
                    AssistantPart::Text { text } => Some(text),
                    _ => None,
                })
                .collect::<String>(),
        ),
        Body::ToolResult { name, content, .. } => (format!("the tool {name}"), content),
    };
    let cut = text.chars().count() > QUOTE_CHARS;
    let shown: String = text.chars().take(QUOTE_CHARS).collect();
    let quoted: Vec<String> = shown.lines().map(|line| format!("> {line}")).collect();
    let mut note = format!(
        "The person's next message replies to message {seq}, written by {author}:\n{}",
        quoted.join("\n")
    );
    if cut {
        note.push_str(&format!(
            "\n(quoted up to here; recall with from_seq {seq} reads it whole)"
        ));
    }
    if question {
        note.push_str(
            "\nIf an answer holds for later work in this project too, propose it with lesson.",
        );
    }
    Ok(Body::User {
        from: Speaker::Harness(REPLY_KIND.to_string()),
        parts: vec![note],
    })
}

/// Unfinished issues shown when a thread starts, most urgent first.
const ISSUES_SHOWN: usize = 5;
const ISSUES_KIND: &str = "issues";

/// The project's most urgent unfinished issues, for the start of a thread
/// (docs/evidence/rules-and-local-issues.md); `None` when there are none.
async fn open_issues(store: &Store, project: Option<ProjectId>) -> Result<Option<Body>, ErrorInfo> {
    let issues = store.issues(project, false, ISSUES_SHOWN).await?;
    if issues.is_empty() {
        return Ok(None);
    }
    let lines: Vec<String> = issues
        .iter()
        .map(|issue| {
            let blocked: Vec<String> = issue
                .blocked_by
                .iter()
                .map(|id| format!("#{}", id.0))
                .collect();
            let blocked = match blocked.is_empty() {
                true => String::new(),
                false => format!(" (blocked by {})", blocked.join(", ")),
            };
            format!(
                "#{} [{}] priority {}: {}{blocked}",
                issue.id.0, issue.state, issue.priority, issue.title
            )
        })
        .collect();
    Ok(Some(Body::User {
        from: Speaker::Harness(ISSUES_KIND.to_string()),
        parts: vec![format!(
            "Unfinished local issues of this project, most urgent first (the issue tool \
             shows more):\n{}",
            lines.join("\n")
        )],
    }))
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
pub(crate) async fn cancelled(cancel: &mut watch::Receiver<bool>) {
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

/// When to use which of the harness's own tools, said once across tools;
/// what each tool does stays in its own description, so the two do not
/// disagree (docs/evidence/tool-guidance.md).
const TOOL_GUIDE: &str = "\n\nThe tools of this harness:\n\
- find and read look things up. explore has a helper search the project and answer \
with file paths and line ranges; it suits finding something that would take many \
searches and reads across files. For one search or one known file, find and read \
are enough. read also shows image files; its region shows a part of one at full detail.\n\
- edit changes one place in a file; edits changes several places, in one file or \
more, all of them or none; write writes a whole file.\n\
- shell runs a command; job types into, waits for or stops a command that is still \
running.\n\
- plan keeps the work list that is shown to you with every request.\n\
- promise keeps the person's standing instructions in their words, shown to you with \
every request: what not to do, when to stop or ask, what may be changed.\n\
- recall reads earlier messages of this thread word for word, also after they were \
summarized, and searches past work and saved knowledge.\n\
- lesson proposes a lesson for later work in this project; method saves a method \
verified to work, for any project.\n\
- search and fetch read the web.\n\
- ask asks the person questions as a form on their screen; the answers come in a \
later message.\n\
- issue keeps the project's local issues across conversations; start the one you work \
on, and close it with the reason when it is done or will not be done.\n\
- camera lists cameras and capture devices, shows their controls, and takes a photo or \
a video after setting them; taking pictures waits for the person.\n\
- Tools named mcp__<server>__<tool> come from the MCP servers the person set up.\n\
- plugin_search finds plugins in the Claude Code and Codex catalogs; plugin_install \
installs one after the person approves.";

/// The system prompt: how to use the tools, the system the commands run
/// on, the reply language, then the project's rules as written, never
/// summarized. The time changes, so it is sent with the state instead
/// (docs/evidence/environment-context.md).
fn system_prompt(rules: Option<&str>, language: Option<&str>, folder: Option<&Path>) -> String {
    let mut system = String::from(
        "You are a coding agent working on a user's project. When a method is shown to \
         work, save it with the method tool. Before investigating a device, format or \
         program from scratch, use recall to look for methods saved in earlier work.",
    );
    system.push_str(TOOL_GUIDE);
    system.push_str(&environment(folder));
    if let Some(language) = language {
        system.push_str("\n\n");
        system.push_str(&crate::language::instruction(language));
    }
    if let Some(rules) = rules {
        system.push_str(" Follow the project's rules below.\n----- RULES -----\n");
        system.push_str(rules);
        system.push_str("\n----- END RULES -----");
    }
    system
}

/// The system and shell the commands run on, and the project folder.
fn environment(folder: Option<&Path>) -> String {
    let mut text = format!(
        "\n\nThe person's computer runs {} ({}). shell runs commands in {}.",
        std::env::consts::OS,
        std::env::consts::ARCH,
        shell::COMMAND_SHELL
    );
    if let Some(folder) = folder {
        text.push_str(&format!(
            " The project folder, where commands run, is {}.",
            folder.display()
        ));
    }
    text
}

/// The person's local time, sent with each request.
fn now(time: chrono::DateTime<chrono::FixedOffset>) -> String {
    format!(
        "Now: {} (the person's local time).",
        time.format("%Y-%m-%d %H:%M %A, UTC%:z")
    )
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
    /// The `ask` calls of this turn, by call id, and their questions.
    asked: Vec<(String, serde_json::Value)>,
    /// Identical failed calls and how often they failed.
    failures: HashMap<(String, String, String), u32>,
    /// Lessons that may apply to this turn's request.
    lessons: Option<String>,
    /// The reply language, repeated after the conversation as in the system
    /// prompt, since the tool output in between is mostly English.
    language: Option<String>,
    mode: Mode,
    /// Held calls the person approved, each run once.
    approved: Vec<(String, String)>,
    /// The model that reviews calls when it is not the turn's own.
    reviewer: Option<std::sync::Arc<Llm>>,
    /// The model that writes compaction summaries when it is not the turn's own.
    compactor: Option<std::sync::Arc<Llm>>,
    /// The model that answers `explore` when it is not the turn's own.
    explorer: Option<std::sync::Arc<Llm>>,
    /// The colleague the person set up, asked when the work looks stuck.
    colleague: Option<std::sync::Arc<Llm>>,
    monitor: crate::colleague::Monitor,
    /// The person's request of this turn, for the colleague.
    request: String,
    /// Where the steps since the colleague's last advice start in
    /// `conversation.messages`, and that advice.
    advised_from: usize,
    last_advice: Option<String>,
    /// Questions the harness put to the person in this turn.
    escalations: Vec<Asked>,
    /// Images the tool being answered shows the model.
    shown: std::sync::Mutex<Vec<crate::conversation::Image>>,
    /// The reviewer's refusals in a row and in this turn.
    denials: (u32, u32),
    /// The thread's goal, as stored when the turn started or last checked.
    goal: Option<crate::goal::Goal>,
    /// The model that checks the goal when it is not the turn's own.
    checker: Option<std::sync::Arc<Llm>>,
    /// Tool calls since the goal was last checked, and the turn's tokens
    /// then.
    goal_calls: u32,
    goal_mark: u64,
    /// The project's git state when the turn started.
    git: Option<String>,
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
        for record in summary_usages {
            usage.add(record.usage);
            usage.records.push(record);
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

    /// The time, the git state, the plan, the person's standing
    /// instructions and the lessons, shown after the history with each
    /// request and never stored, so they survive compaction and keep the
    /// cached prefix intact.
    async fn state(&self) -> Result<Option<String>, ErrorInfo> {
        let plan = agent_tools::render_plan(self.store, self.session).await?;
        let latest = self
            .seqs
            .iter()
            .zip(&self.conversation.messages)
            .rev()
            .find(|(_, message)| crate::promise::person_text(message).is_some())
            .map(|(seq, _)| *seq);
        let promises = crate::promise::render(self.store, self.session, latest).await?;
        let jobs = shell::render_jobs(&self.store.files_folder().join("logs"), self.session);
        let language = self.language.as_deref().map(crate::language::instruction);
        let skills = crate::plugins::render_skills(&crate::plugins::folder(self.store));
        let goal = self
            .goal
            .as_ref()
            .filter(|goal| goal.is_active())
            .map(crate::goal::Goal::render);
        let now = now(chrono::Local::now().fixed_offset());
        let parts: Vec<&str> = [
            Some(now.as_str()),
            self.git.as_deref(),
            goal.as_deref(),
            plan.as_deref(),
            jobs.as_deref(),
            self.lessons.as_deref(),
            skills.as_deref(),
            promises.as_deref(),
            language.as_deref(),
        ]
        .into_iter()
        .flatten()
        .collect();
        Ok(Some(parts.join("\n")))
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
            seq: self.next_seq,
            events: Some(&self.events),
            shown: &self.shown,
        };
        if name == crate::goal::GOAL {
            return self.claim(arguments).await;
        }
        if name == crate::explore::EXPLORE {
            let explorer = self.explorer.clone();
            let model = explorer.as_deref().unwrap_or(llm);
            return crate::explore::run(model, &context, arguments, usage)
                .await
                .map_or_else(|message| (message, true), |text| (text, false));
        }
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
        if name == crate::plugins::INSTALL {
            return Some("plugins are installed only with the person's approval".into());
        }
        if name == crate::camera::CAMERA && crate::camera::takes_pictures(arguments) {
            return Some("photos and videos are taken only with the person's approval".into());
        }
        if self.sandboxed() && command.as_ref().is_some_and(|c| c.outside_sandbox) {
            return Some("it asks to run outside the sandbox".into());
        }
        let reads = command
            .as_ref()
            .is_some_and(|c| shell::reads_only(&c.command));
        // What an MCP tool does is not known here (its annotations are
        // untrusted), so a call to one is treated as a change.
        let mcp = name.starts_with(crate::mcp::PREFIX);
        let changes = matches!(
            name,
            crate::edit::EDIT | crate::edit::EDITS | crate::edit::WRITE
        ) || (name == shell::SHELL && !reads)
            || mcp;
        match self.mode {
            Mode::Ask if changes => Some("every change waits for the person in this mode".into()),
            Mode::Auto if mcp => Some("calls to MCP tools are reviewed in this mode".into()),
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
        // The camera and plugin installs go to the person, never to the
        // reviewer.
        if self.mode == Mode::Auto
            && name != crate::camera::CAMERA
            && name != crate::plugins::INSTALL
        {
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
                self.reviewer.as_deref().unwrap_or(llm),
                &seen,
                self.folder.as_deref(),
                self.rules.as_deref(),
                name,
                arguments,
            )
            .await;
            if let Some(spent) = spent {
                usage.add(spent.as_ref().and_then(|record| record.usage));
                usage.records.extend(spent);
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

    /// Has the colleague advise on the steps since its last advice, and
    /// adds the advice to the thread. A colleague that cannot be asked is
    /// reported and the work goes on without it.
    async fn ask_colleague(
        &mut self,
        call: crate::colleague::Call,
        cancel: &mut watch::Receiver<bool>,
        usage: &mut TurnUsage,
    ) -> Result<(), ErrorInfo> {
        match call {
            crate::colleague::Call::Colleague(sign) => {
                let from = self.advised_from.min(self.conversation.messages.len());
                let asked = crate::colleague::about_steps(
                    &self.request,
                    &self.conversation.messages[from..],
                    self.last_advice.as_deref(),
                    &sign,
                );
                if let Some(advice) = self.hear_colleague(asked, cancel, usage).await? {
                    self.last_advice = Some(advice);
                }
            }
            crate::colleague::Call::Person(sign) => {
                let question = format!(
                    "The work still looks stuck after {} rounds of the colleague's advice \
                     in this turn. {} How should it go on?",
                    crate::colleague::ADVICE_PER_TURN,
                    sign.hint()
                );
                self.escalate(&question, &["Keep going", "Stop and wait for my direction"])
                    .await?;
            }
        }
        Ok(())
    }

    /// Asks the colleague and adds its advice to the thread; the advice, or
    /// `None` when the colleague could not be asked, which is reported and
    /// the work goes on without it.
    async fn hear_colleague(
        &mut self,
        asked: (String, String),
        cancel: &mut watch::Receiver<bool>,
        usage: &mut TurnUsage,
    ) -> Result<Option<String>, ErrorInfo> {
        let Some(colleague) = self.colleague.clone() else {
            return Ok(None);
        };
        match crate::colleague::advise(&colleague, asked, cancel, usage).await {
            Ok((message, advice)) => {
                self.record(vec![message]).await?;
                self.advised_from = self.conversation.messages.len();
                Ok(Some(advice))
            }
            Err(error) => {
                eprintln!("inocode: the colleague could not be asked: {error}");
                Ok(None)
            }
        }
    }

    /// Puts a question to the person: it is added to the thread, so the
    /// model knows and the person can reply to it, and reported when the
    /// turn ends with the questions of `ask`.
    async fn escalate(&mut self, question: &str, options: &[&str]) -> Result<(), ErrorInfo> {
        // A goal runs without asking the person; its limits stop it instead.
        if self.goal_active() {
            return Ok(());
        }
        let note = format!(
            "The harness asked the person how to go on: {question} Their answer comes in a \
             later message. Go on with work that does not depend on it; if none is left, end \
             your answer and say what you are waiting for."
        );
        self.record(vec![Body::User {
            from: Speaker::Harness(ESCALATION_KIND.to_string()),
            parts: vec![note],
        }])
        .await?;
        let options: Vec<serde_json::Value> = options
            .iter()
            .map(|label| serde_json::json!({ "label": label }))
            .collect();
        self.escalations.push(Asked {
            seq: self.seqs.last().copied(),
            questions: serde_json::json!([{
                "question": question,
                "kind": "choose_one",
                "options": options
            }]),
        });
        Ok(())
    }

    /// At the start of a turn: when the issue this thread works on was left
    /// unfinished by the last turns, the colleague is asked about it, or,
    /// after its advice did not help either, the person.
    async fn check_issue(
        &mut self,
        cancel: &mut watch::Receiver<bool>,
        usage: &mut TurnUsage,
    ) -> Result<(), ErrorInfo> {
        use crate::colleague::{ISSUE_ADVICE, UNFINISHED, UNFINISHED_TURNS};
        if self.colleague.is_none() {
            return Ok(());
        }
        let Some(id) = self.store.current_issue(self.session).await? else {
            return Ok(());
        };
        let Some((issue, body)) = self.store.issue(self.project, id).await? else {
            return Ok(());
        };
        let events = self.store.events(id).await?;
        let started = events
            .iter()
            .rposition(|event| event.event == "in_progress")
            .map_or(0, |index| index + 1);
        let since = events
            .iter()
            .rposition(|event| {
                ["in_progress", "advised", "escalated"].contains(&event.event.as_str())
            })
            .map_or(0, |index| index + 1);
        let unfinished: Vec<String> = events[since..]
            .iter()
            .filter(|event| event.event == UNFINISHED)
            .map(|event| event.detail.clone())
            .collect();
        if unfinished.len() < UNFINISHED_TURNS {
            return Ok(());
        }
        let advice: Vec<&str> = events[started..]
            .iter()
            .filter(|event| event.event == "advised")
            .map(|event| event.detail.as_str())
            .collect();
        let at = Some(crate::rules::At {
            session: self.session,
            seq: self.seqs.last().copied().unwrap_or(0),
        });
        if advice.len() >= ISSUE_ADVICE {
            let question = format!(
                "Issue #{} \"{}\" is still unfinished after {} rounds of the colleague's \
                 advice and {} more turns. How should it go on?",
                id.0,
                issue.title,
                advice.len(),
                unfinished.len()
            );
            self.escalate(
                &question,
                &[
                    "Keep working on it",
                    "Set it aside",
                    "Wait for my direction",
                ],
            )
            .await?;
            self.store
                .record(id, "escalated", crate::rules::Actor::Harness, at, "")
                .await?;
            return Ok(());
        }
        let asked = crate::colleague::about_issue(
            &self.request,
            &format!("#{} {}\n{body}", id.0, issue.title),
            &unfinished,
            advice.last().copied(),
        );
        if let Some(given) = self.hear_colleague(asked, cancel, usage).await? {
            self.store
                .record(id, "advised", crate::rules::Actor::Harness, at, &given)
                .await?;
        }
        Ok(())
    }

    /// At the end of a turn: when the issue this thread works on is still
    /// unfinished, the harness records the turn as an unfinished attempt,
    /// with how it ended.
    async fn record_attempt(&self, ending: &Ending) {
        let Ok(Some(id)) = self.store.current_issue(self.session).await else {
            return;
        };
        let said: String = self
            .conversation
            .messages
            .iter()
            .rev()
            .find_map(|message| match &message.body {
                Body::Assistant { parts, .. } => {
                    let text: String = parts
                        .iter()
                        .filter_map(|part| match part {
                            AssistantPart::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect();
                    (!text.trim().is_empty()).then_some(text)
                }
                _ => None,
            })
            .unwrap_or_default();
        let said: String = said.trim().chars().take(ATTEMPT_CHARS).collect();
        let detail = format!("The turn ended ({}). Last words: {said}", ending.as_str());
        let at = Some(crate::rules::At {
            session: self.session,
            seq: self.seqs.last().copied().unwrap_or(0),
        });
        let recorded = self
            .store
            .record(
                id,
                crate::colleague::UNFINISHED,
                crate::rules::Actor::Harness,
                at,
                &detail,
            )
            .await;
        if let Err(error) = recorded {
            eprintln!(
                "inocode: the unfinished turn was not recorded: {}",
                error.message
            );
        }
    }

    fn goal_active(&self) -> bool {
        self.goal.as_ref().is_some_and(crate::goal::Goal::is_active)
    }

    /// Keeps the working model's claim for the next check; deciding is the
    /// checker's.
    async fn claim(&mut self, arguments: &str) -> Answer {
        let claim = match crate::goal::claim(arguments) {
            Ok(claim) => claim,
            Err(error) => return (error, true),
        };
        let goal = match crate::goal::load(self.store, self.session).await {
            Ok(Some(goal)) if goal.is_active() => goal,
            Ok(_) => return ("This thread has no active goal.".to_string(), true),
            Err(error) => return (error.message, true),
        };
        let goal = crate::goal::Goal {
            claim: Some(claim),
            ..goal
        };
        if let Err(error) = crate::goal::save(self.store, self.session, &goal).await {
            return (error.message, true);
        }
        self.goal = Some(goal);
        (
            "Recorded as a claim. End your answer; the checker decides from the evidence."
                .to_string(),
            false,
        )
    }

    /// After the model ended its answer with a goal active: has the checker
    /// judge it and records what follows. `true` when the turn goes on.
    async fn pursue(&mut self, llm: &Llm, usage: &mut TurnUsage) -> Result<bool, ErrorInfo> {
        // Read again: the person may have stopped or replaced the goal.
        let goal = crate::goal::load(self.store, self.session).await?;
        let Some(mut goal) = goal.filter(crate::goal::Goal::is_active) else {
            self.goal = None;
            return Ok(false);
        };
        let work: Vec<(i64, Message)> = self
            .seqs
            .iter()
            .copied()
            .zip(self.conversation.messages.iter().cloned())
            .filter(|(seq, _)| *seq >= goal.from_seq)
            .collect();
        let checker = self.checker.clone();
        let context = agent_tools::Context {
            store: self.store,
            session: self.session,
            project: self.project,
            rules: self.rules.as_deref(),
            folder: self.folder.as_deref(),
            cancel: self.cancel.clone(),
            sandboxed: self.sandboxed(),
            seq: self.next_seq,
            events: None,
            shown: &self.shown,
        };
        let checked = crate::goal::check(
            checker.as_deref().unwrap_or(llm),
            &context,
            &goal,
            &work,
            usage,
        )
        .await;
        let spent = (usage.input + usage.output).saturating_sub(self.goal_mark);
        self.goal_mark = usage.input + usage.output;
        let (text, goes_on) = match checked {
            Ok(verdict) => {
                let after = crate::goal::apply(&mut goal, verdict, self.goal_calls > 0, spent);
                self.goal_calls = 0;
                crate::goal::save(self.store, self.session, &goal).await?;
                match after {
                    crate::goal::After::Continue(text) => (text, true),
                    crate::goal::After::End(text) => (text, false),
                }
            }
            Err(why) => (
                format!("The checker could not decide ({why}); the goal stays active."),
                false,
            ),
        };
        self.goal = Some(goal);
        self.record(vec![Body::User {
            from: Speaker::Harness(crate::goal::GOAL_KIND.to_string()),
            parts: vec![text],
        }])
        .await?;
        Ok(goes_on)
    }

    /// The stored position of the answer to call `call`.
    fn seq_of_result(&self, call: &str) -> Option<i64> {
        self.conversation
            .messages
            .iter()
            .zip(&self.seqs)
            .find(|(message, _)| {
                matches!(&message.body, Body::ToolResult { call_id, .. } if call_id == call)
            })
            .map(|(_, seq)| *seq)
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
                    if name != crate::goal::GOAL {
                        self.goal_calls += 1;
                    }
                    let answer = self.answer(llm, body, name, arguments, usage).await;
                    if name == crate::ask::ASK && !answer.1 {
                        if let Ok((questions, _)) = crate::ask::check(arguments) {
                            self.asked.push((call_id.clone(), questions));
                        }
                    }
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
            let images = std::mem::take(&mut *self.shown.lock().expect("shown lock"));
            results.push(Body::ToolResult {
                call_id: call_id.clone(),
                name: name.clone(),
                content: answer.0,
                is_error: answer.1,
                images,
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
    prompt: Prompt,
    approvals: Approvals,
    mut steer: mpsc::UnboundedReceiver<String>,
    mut cancel: watch::Receiver<bool>,
    events: mpsc::Sender<TurnEvent>,
) -> Outcome {
    let mut usage = TurnUsage::default();
    let models = reviewer(llm, approvals.mode).and_then(|reviewer| {
        let compactor = compactor(llm)?;
        Ok((reviewer, compactor, explorer(llm)?))
    });
    let (reviewer, compactor, explorer) = match models {
        Ok(models) => models,
        Err(error) => {
            return Outcome {
                ending: Ending::Failed(error),
                usage,
                held: Vec::new(),
                questions: Vec::new(),
            }
        }
    };
    let mut thread = match open(
        store,
        session,
        &prompt.text,
        approvals,
        cancel.clone(),
        events,
    )
    .await
    {
        Ok(mut thread) => {
            thread.reviewer = reviewer;
            thread.compactor = compactor;
            thread.explorer = explorer;
            match colleague(llm, store).await {
                Ok(colleague) => thread.colleague = colleague,
                Err(error) => {
                    return Outcome {
                        ending: Ending::Failed(error),
                        usage,
                        held: Vec::new(),
                        questions: Vec::new(),
                    }
                }
            }
            match other_model(llm, CHECK_MODEL_ENV) {
                Ok(checker) => thread.checker = checker.or_else(|| thread.colleague.clone()),
                Err(error) => {
                    return Outcome {
                        ending: Ending::Failed(error),
                        usage,
                        held: Vec::new(),
                        questions: Vec::new(),
                    }
                }
            }
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
                questions: Vec::new(),
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
    thread.record_attempt(&ending).await;
    if let Err(error) = store.log_requests(session, &usage.records).await {
        eprintln!("inocode: the requests were not logged: {}", error.message);
    }
    let mut questions: Vec<Asked> = thread
        .asked
        .iter()
        .map(|(call, questions)| Asked {
            seq: thread.seq_of_result(call),
            questions: questions.clone(),
        })
        .collect();
    questions.append(&mut thread.escalations);
    Outcome {
        ending,
        usage,
        held: thread.held,
        questions,
    }
}

/// The model that reviews calls in the `auto` mode when it is not the
/// turn's own: the one `INOCODE_REVIEW_MODEL` names, on the same provider.
fn reviewer(llm: &Llm, mode: Mode) -> Result<Option<std::sync::Arc<Llm>>, ErrorInfo> {
    if mode != Mode::Auto {
        return Ok(None);
    }
    match std::env::var(REVIEW_MODEL_ENV) {
        Ok(model) if !model.trim().is_empty() => llm.sibling(model.trim()).map(Some),
        _ => Ok(None),
    }
}

/// The model that writes compaction summaries when it is not the turn's
/// own: the one `INOCODE_COMPACT_MODEL` names, on the same provider.
fn compactor(llm: &Llm) -> Result<Option<std::sync::Arc<Llm>>, ErrorInfo> {
    other_model(llm, COMPACT_MODEL_ENV)
}

/// The model that answers `explore` when it is not the turn's own: the one
/// `INOCODE_EXPLORE_MODEL` names, on the same provider.
fn explorer(llm: &Llm) -> Result<Option<std::sync::Arc<Llm>>, ErrorInfo> {
    other_model(llm, EXPLORE_MODEL_ENV)
}

/// The colleague named in `settings.json` beside the database, if any.
async fn colleague(llm: &Llm, store: &Store) -> Result<Option<std::sync::Arc<Llm>>, ErrorInfo> {
    let settings = store.files_folder().with_file_name("settings.json");
    let configured = crate::colleague::configured(&settings)
        .await
        .map_err(|error| {
            ErrorInfo::new(
                "colleague_config",
                format!("{}: {error}", settings.display()),
            )
        })?;
    configured
        .map(|colleague| {
            llm.other(
                colleague.provider.as_deref(),
                &colleague.model,
                colleague.base_url.as_deref(),
            )
        })
        .transpose()
}

fn other_model(llm: &Llm, variable: &str) -> Result<Option<std::sync::Arc<Llm>>, ErrorInfo> {
    match std::env::var(variable) {
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
    let language =
        crate::language::default(&store.files_folder().with_file_name("settings.json")).await;
    let goal = crate::goal::load(store, session).await?;
    let system = system_prompt(rules.as_deref(), language.as_deref(), folder.as_deref());
    let git = match folder.as_deref() {
        Some(folder) => crate::git::state(folder).await,
        None => None,
    };
    Ok(Thread {
        store,
        session,
        project,
        folder,
        cancel,
        conversation: Conversation {
            system: Some(system),
            messages,
        },
        seqs,
        next_seq,
        turn_start: next_seq,
        rules,
        events,
        allowed: approvals.rules.into_iter().collect(),
        held: Vec::new(),
        asked: Vec::new(),
        failures: HashMap::new(),
        lessons,
        language,
        mode: approvals.mode,
        approved: approvals.calls,
        reviewer: None,
        compactor: None,
        explorer: None,
        colleague: None,
        monitor: Default::default(),
        request: String::new(),
        advised_from: 0,
        last_advice: None,
        escalations: Vec::new(),
        shown: std::sync::Mutex::new(Vec::new()),
        denials: (0, 0),
        goal,
        checker: None,
        goal_calls: 0,
        goal_mark: 0,
        git,
    })
}

async fn steps(
    llm: &Llm,
    thread: &mut Thread<'_>,
    prompt: Prompt,
    steer: &mut mpsc::UnboundedReceiver<String>,
    cancel: &mut watch::Receiver<bool>,
    usage: &mut TurnUsage,
) -> Ending {
    let mut harness: Vec<ToolSpec> = agent_tools::specs(thread.sandboxed());
    // Under a goal the model does not ask the person; it can claim instead.
    if thread.goal_active() {
        harness.retain(|tool| tool.name != crate::ask::ASK);
        harness.push(crate::goal::spec());
    }
    let mut opening = Vec::new();
    if thread.conversation.messages.is_empty() {
        match open_issues(thread.store, thread.project).await {
            Ok(Some(issues)) => opening.push(issues),
            Ok(None) => {}
            Err(error) => return Ending::Failed(error),
        }
    }
    if let Some(seq) = prompt.reply_to {
        match quote(thread.store, thread.session, seq).await {
            Ok(quoted) => opening.push(quoted),
            Err(error) => return Ending::Failed(error),
        }
    }
    thread.request = prompt.text.clone();
    thread.advised_from = thread.conversation.messages.len();
    opening.push(person(prompt.text));
    if let Err(error) = thread.record(opening).await {
        return Ending::Failed(error);
    }
    if let Err(error) = thread.check_issue(cancel, usage).await {
        return Ending::Failed(error);
    }
    let mut cuts = 0;
    loop {
        // MCP tools found by a search are offered from the next request on.
        let mut tools = harness.clone();
        tools.extend(crate::mcp::specs(thread.store, &thread.conversation.messages).await);
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
        usage.add_stream(&stream, "turn");
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
        let sign = match &thread.colleague {
            Some(_) => thread.monitor.step(&body),
            None => None,
        };
        if let Err(error) = thread.record(bodies).await {
            return Ending::Failed(error);
        }
        if let (Some(call), Some(Step::Continue)) = (sign, &step) {
            if let Err(error) = thread.ask_colleague(call, cancel, usage).await {
                return Ending::Failed(error);
            }
        }
        match step {
            None => return Ending::Failed(failure.expect("no step only after a failure")),
            Some(Step::Continue) => cuts = 0,
            Some(Step::Resume) => cuts += 1,
            Some(Step::Finish(Ending::Completed)) if thread.goal_active() => {
                let pursued = tokio::select! {
                    pursued = thread.pursue(llm, usage) => Some(pursued),
                    () = cancelled(cancel) => None,
                };
                match pursued {
                    Some(Ok(true)) => cuts = 0,
                    Some(Ok(false)) => return Ending::Completed,
                    Some(Err(error)) => return Ending::Failed(error),
                    None => return Ending::Interrupted,
                }
            }
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
        let system = system_prompt(Some("- テストは全部実行する"), None, None);
        assert!(
            system.ends_with("----- RULES -----\n- テストは全部実行する\n----- END RULES -----")
        );
        let bare = system_prompt(None, None, None);
        assert!(bare.starts_with("You are a coding agent working on a user's project."));
        assert!(!bare.contains("RULES"));
    }

    #[test]
    fn the_tool_guide_names_every_tool_of_the_harness() {
        for sandboxed in [false, true] {
            for spec in agent_tools::specs(sandboxed) {
                let named = format!(" {} ", spec.name);
                let listed = TOOL_GUIDE.contains(&format!("- {} ", spec.name))
                    || TOOL_GUIDE.contains(&named)
                    || TOOL_GUIDE.contains(&format!(" {};", spec.name));
                assert!(listed, "{} is not in the tool guide", spec.name);
            }
        }
        assert!(system_prompt(None, None, None).contains(TOOL_GUIDE));
    }

    #[test]
    fn the_model_is_told_the_system_shell_and_folder() {
        let folder = Path::new("project");
        let system = system_prompt(None, None, Some(folder));
        assert!(system.contains(&format!("runs {} (", std::env::consts::OS)));
        assert!(system.contains(shell::COMMAND_SHELL));
        assert!(system.contains(&format!("is {}.", folder.display())));
        assert_eq!(cfg!(windows), shell::COMMAND_SHELL.contains("PowerShell"));
        assert!(!system_prompt(None, None, None).contains("project folder"));
    }

    #[test]
    fn the_time_names_the_day_and_the_offset() {
        let time = chrono::DateTime::parse_from_rfc3339("2026-11-23T14:05:09+09:00").unwrap();
        assert_eq!(
            now(time),
            "Now: 2026-11-23 14:05 Monday, UTC+09:00 (the person's local time)."
        );
    }

    #[test]
    fn the_reply_language_comes_before_the_rules() {
        let system = system_prompt(Some("- rule"), Some("ja-JP"), None);
        let language = system.find("in ja-JP").unwrap();
        assert!(language < system.find("----- RULES -----").unwrap());
        assert!(!system_prompt(None, None, None).contains("Write your replies"));
    }
}
