//! The turn loop the server runs for a thread: request, record, run the
//! tool calls, and decide whether to go on. Design and sources:
//! docs/evidence/turn-protocol.md and docs/evidence/agent-tools.md.

use std::collections::{HashMap, HashSet};
use std::future::pending;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
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
    /// The model's thinking as the provider streams it; the finished
    /// thinking is in the stored message.
    Thinking(String),
    /// A piece of the model's thinking translated into the person's
    /// language, when that is turned on; shown only, never stored.
    ThinkingTranslation(crate::translate::Translated),
    /// A message now stored in the thread.
    Message { seq: i64, message: Message },
    /// A question for the person, answered with `turn/answer` while the
    /// call waits: an MCP server's, or whether a call may run (`mode`
    /// `approval`).
    Elicitation(serde_json::Value),
    /// The question of an `Elicitation` with this id is settled: answered,
    /// or no longer waited for.
    Answered(u64),
    /// How far an MCP tool call has come, as its server reports it.
    McpProgress(serde_json::Value),
    /// A log message an MCP server sent during a call.
    McpLog(serde_json::Value),
    /// What is left of the task by the estimate, at the start and after
    /// each model request (docs/evidence/effort-estimates.md).
    Estimate(crate::estimate::Estimate),
    /// One model request of the turn ended: what it used and how fast, the
    /// turn's totals so far, and how full the context is.
    Usage(UsageReport),
    /// The person's command started as job `u64`; its output can be
    /// watched with the thread's jobs while the turn waits for it.
    ShellStarted(u64),
    /// The thread's new title.
    Title(String),
    /// A title the model proposes for the thread; the title is unchanged.
    TitleProposed(String),
    /// What changed in the project's files since the copy taken before the
    /// turn, as `{seq, files, text, truncated}`
    /// (docs/evidence/turn-diff.md).
    Diff(serde_json::Value),
    /// Plan mode of the thread is now on or off
    /// (docs/evidence/plan-mode.md).
    PlanMode(bool),
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

/// What [`TurnEvent::Usage`] tells about a request.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UsageReport {
    pub purpose: &'static str,
    pub provider: String,
    pub model: String,
    pub input: Option<u64>,
    pub output: Option<u64>,
    pub cached: Option<u64>,
    pub first_output_ms: Option<u64>,
    pub duration_ms: u64,
    /// Output tokens per second after the first output, or over the whole
    /// request when it had no text.
    pub output_per_second: Option<f64>,
    pub outcome: &'static str,
    /// The turn's requests and tokens so far, this one included.
    pub total: UsageTotal,
    /// The input of the latest request of the turn's own model, which is
    /// what the conversation fills of its context.
    pub context_used: Option<u64>,
    pub context_window: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UsageTotal {
    pub requests: u64,
    pub input: u64,
    pub output: u64,
    pub cached: u64,
}

impl UsageReport {
    fn new(
        record: &crate::RequestRecord,
        total: &TurnUsage,
        context_used: Option<u64>,
        context_window: Option<u64>,
    ) -> Self {
        let generating = match record.first_output_ms {
            Some(first) => record.duration_ms.saturating_sub(first),
            None => record.duration_ms,
        };
        let output = record.usage.map(|usage| usage.output);
        Self {
            purpose: record.purpose,
            provider: record.provider.clone(),
            model: record.model.clone(),
            input: record.usage.map(|usage| usage.input),
            output,
            cached: record.usage.map(|usage| usage.cached),
            first_output_ms: record.first_output_ms,
            duration_ms: record.duration_ms,
            output_per_second: output
                .filter(|_| generating > 0)
                .map(|output| output as f64 * 1000.0 / generating as f64),
            outcome: record.outcome,
            total: UsageTotal {
                requests: total.requests,
                input: total.input,
                output: total.output,
                cached: total.cached,
            },
            context_used,
            context_window,
        }
    }
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

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::OnRequest => "on-request",
            Self::Auto => "auto",
            Self::Full => "full",
        }
    }
}

/// What the person allowed for one turn: its mode.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Approvals {
    pub mode: Mode,
}

/// How far the person's approval of a call reaches: this call only, or
/// from now on every call with the same tool and arguments in the thread,
/// or in every thread of its project.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scope {
    Once,
    Thread,
    Project,
}

impl Scope {
    /// The scope of an accepted approval prompt's `content`; `once` when
    /// it names none.
    pub(crate) fn of(content: Option<&serde_json::Value>) -> Option<Self> {
        match content.and_then(|content| content.get("scope")) {
            None | Some(serde_json::Value::Null) => Some(Self::Once),
            Some(scope) => match scope.as_str()? {
                "once" => Some(Self::Once),
                "thread" => Some(Self::Thread),
                "project" => Some(Self::Project),
                _ => None,
            },
        }
    }
}

/// The `mode` of the elicitation that asks the person to approve a call.
pub(crate) const APPROVAL_MODE: &str = "approval";

/// Refusals of the reviewer in a row, and in a turn, after which calls go
/// to the person instead (the counts of Claude Code and DeepAI Code).
const DENIALS_IN_A_ROW: u32 = 3;
const DENIALS_PER_TURN: u32 = 20;

/// How the person answered a question put to them during the turn.
enum Heard {
    /// The question could not be shown: nobody receives the turn's events.
    Nobody,
    /// The turn was stopped before an answer came.
    Stopped,
    Answer(serde_json::Value),
}

/// Whether plan mode refuses the call: it changes files, installs, hands
/// work on, or captures pictures or sound.
fn plan_refuses(name: &str, arguments: &str) -> bool {
    matches!(
        name,
        crate::edit::EDIT
            | crate::edit::EDITS
            | crate::edit::WRITE
            | crate::plugins::INSTALL
            | crate::company::DELEGATE
    ) || (name == crate::camera::CAMERA && crate::camera::takes_pictures(arguments))
        || (name == crate::audio::AUDIO && crate::audio::records(arguments))
}

/// Whether the call waits for the person in plan mode, whatever the
/// approval mode: a command that may change something or leave the
/// sandbox, input to a running command, or an MCP tool.
fn plan_asks(name: &str, arguments: &str) -> bool {
    match name {
        shell::SHELL => shell::shell_arguments(arguments)
            .is_none_or(|command| command.outside_sandbox || !shell::reads_only(&command.command)),
        shell::JOB => shell::job_types(arguments),
        _ => name.starts_with(crate::mcp::PREFIX),
    }
}

/// Why a call waits, and whether only the person may let it run (the
/// reviewer of the `auto` mode never decides it).
struct Need {
    reason: String,
    person: bool,
}

/// Whether the arguments of a call are the ones the person approved: the
/// same JSON value, whatever its spacing and key order.
fn same_arguments(approved: &str, arguments: &str) -> bool {
    match (
        serde_json::from_str::<serde_json::Value>(approved),
        serde_json::from_str::<serde_json::Value>(arguments),
    ) {
        (Ok(approved), Ok(arguments)) => approved == arguments,
        _ => approved == arguments,
    }
}

/// The models of a turn. `main` is the conversation's model: a change of
/// the thread's settings sends a new one, used from the next request on.
/// A helper role that is `None` is done by the conversation's model.
pub struct Models {
    pub main: watch::Receiver<std::sync::Arc<Llm>>,
    /// Reviews calls in the `auto` mode.
    pub reviewer: Option<std::sync::Arc<Llm>>,
    pub compactor: Option<std::sync::Arc<Llm>>,
    pub explorer: Option<std::sync::Arc<Llm>>,
    /// Checks goals; the colleague when `None` and there is one
    /// (docs/evidence/goal-command.md).
    pub checker: Option<std::sync::Arc<Llm>>,
    pub colleague: Option<std::sync::Arc<Llm>>,
    /// An approval mode the person chose while the turn runs, used from
    /// the next request on.
    pub mode: watch::Receiver<Option<Mode>>,
    /// The members the turn may hand work to or ask for review
    /// (docs/evidence/company.md).
    pub company: Option<std::sync::Arc<crate::company::Company>>,
    /// Set for a member's run of a delegated task: the tokens it may use.
    /// Such a run cannot delegate further.
    pub budget: Option<u64>,
    /// How the thread is named from the person's first message; `None`
    /// for a run that does not name its thread.
    pub title: Option<crate::title::Titling>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub ending: Ending,
    pub usage: TurnUsage,
    pub questions: Vec<Asked>,
    /// What went wrong beside the work without ending the turn, such as a
    /// colleague that could not be asked or requests that were not logged.
    pub warnings: Vec<String>,
    /// The files the edit tools named.
    pub edited: Vec<String>,
    /// The turn was stopped for using up its token budget.
    pub over_budget: bool,
}

/// Questions the model asked the person with `ask`, as the form they make.
/// `seq` is the stored answer of the call, which a reply to the questions
/// names; `None` when it is not in the thread's history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Asked {
    pub seq: Option<i64>,
    pub questions: serde_json::Value,
}

/// What the person sent to start a turn: the text, the earlier message of
/// the thread it replies to, if any (its seq), and the files sent with it.
#[derive(Debug, Clone, Default)]
pub struct Prompt {
    pub text: String,
    pub reply_to: Option<i64>,
    pub attached: crate::attach::Attached,
    /// A command the person runs; its output becomes the prompt
    /// (docs/evidence/person-shell.md).
    pub shell: Option<String>,
}

/// Characters of a replied-to message that are quoted; the rest is read
/// with `recall`.
const QUOTE_CHARS: usize = 2_000;
pub(crate) const REPLY_KIND: &str = "reply";
const BUDGET_KIND: &str = "budget";
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
            ..
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
        images: Vec::new(),
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
        images: Vec::new(),
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
- audio lists the devices that record sound and records one to a wav or flac file in \
the format asked for, after the person approves; transcribe turns speech in an audio \
file into text through the person's Claude or ChatGPT sign-in, or Google's speech \
service.\n\
- exit_plan, offered only in plan mode, presents your plan to the person for approval.\n\
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
    /// See [`Outcome::warnings`].
    warnings: Vec<String>,
    /// The project's git state when the turn started.
    git: Option<String>,
    /// Whether and how the thinking is translated.
    thinking_translation: ThinkingTranslation,
    /// The tool calls of the turn, for its task record.
    tally: crate::estimate::Tally,
    /// The estimate re-made after each request, with the tokens the task
    /// had used before this turn and when the turn started
    /// (docs/evidence/effort-estimates.md).
    forecast: Option<(crate::estimate::Forecast, u64, std::time::Instant)>,
    company: Option<std::sync::Arc<crate::company::Company>>,
    /// See [`Models::budget`].
    budget: Option<u64>,
    over_budget: bool,
    /// No message of the thread before this turn is the person's.
    first: bool,
    /// The titles the model proposed in this thread.
    proposed: HashSet<String>,
    /// Plan mode, as the thread's settings said at the latest request.
    plan: bool,
    /// The usage of the automatic title's requests, when it was started.
    title_spent: Option<crate::title::Spent>,
    /// How many of the turn's request records were reported.
    reported: usize,
    /// See [`UsageReport::context_used`].
    context_used: Option<u64>,
}

/// The translation of the thinking as the settings last read give it: one
/// stream of translation for the whole turn, so that a model request never
/// waits for it.
struct ThinkingTranslation {
    settings: PathBuf,
    language: Option<String>,
    /// [`crate::settings::written`] when the settings were read.
    read_at: u64,
    /// The translator and language the thinking is translated into, when
    /// that is turned on.
    current: Option<(std::sync::Arc<crate::translate::Translator>, String)>,
    events: mpsc::Sender<TurnEvent>,
    streamer: Option<crate::translate::Streamer<TurnEvent>>,
    /// Streams of translations turned off or changed during the turn: they
    /// go on translating what they hold until the turn ends.
    retired: Vec<crate::translate::Streamer<TurnEvent>>,
}

impl ThinkingTranslation {
    async fn read(
        settings: PathBuf,
        language: Option<String>,
        events: mpsc::Sender<TurnEvent>,
    ) -> Self {
        let read_at = crate::settings::written();
        let current = thinking_translator(&settings, language.as_deref()).await;
        Self {
            settings,
            language,
            read_at,
            current,
            events,
            streamer: None,
            retired: Vec::new(),
        }
    }

    /// Reads the settings again when this process wrote them since; `true`
    /// when the translation they give changed. A stream of the translation
    /// that changed is not dropped: what it holds is still translated and
    /// sent, while the turn goes on.
    async fn refresh(&mut self) -> bool {
        if crate::settings::written() == self.read_at {
            return false;
        }
        self.read_at = crate::settings::written();
        let now = thinking_translator(&self.settings, self.language.as_deref()).await;
        let same = match (&self.current, &now) {
            (None, None) => true,
            (Some((was, was_to)), Some((is, is_to))) => {
                std::sync::Arc::ptr_eq(was, is) && was_to == is_to
            }
            _ => false,
        };
        if same {
            return false;
        }
        self.current = now;
        if let Some(streamer) = self.streamer.take() {
            streamer.flush();
            self.retired.push(streamer);
        }
        true
    }

    /// Adds a piece of the model's thinking.
    fn push(&mut self, text: &str) {
        if self.streamer.is_none() {
            self.streamer = self.current.clone().map(|(translator, to)| {
                crate::translate::Streamer::start(
                    translator,
                    to,
                    self.events.clone(),
                    TurnEvent::ThinkingTranslation,
                )
            });
        }
        if let Some(streamer) = &self.streamer {
            streamer.push(text);
        }
    }

    /// The model's answer ended: its last unfinished sentence is
    /// translated on its own.
    fn answer_ended(&self) {
        if let Some(streamer) = &self.streamer {
            streamer.flush();
        }
    }

    /// The turn ended: waits, at most `wait`, for what is left to be
    /// translated, so that it comes before the turn's end; what is not
    /// translated by then is sent as written, with an error.
    async fn end(&mut self, wait: std::time::Duration) {
        let finishing: Vec<_> = self
            .retired
            .drain(..)
            .chain(self.streamer.take())
            .map(|streamer| tokio::spawn(streamer.finish(wait)))
            .collect();
        for streamer in finishing {
            let _ = streamer.await;
        }
    }
}

/// How long a turn waits, after it ends, for the rest of its thinking to be
/// translated: as long as one request to an online translation service may
/// take, so that one under way can still answer.
const TRANSLATION_WAIT: std::time::Duration = crate::translate::WAIT;

/// The translator for the thinking when the settings at `path` turn it on
/// and a language to translate into is known.
async fn thinking_translator(
    path: &Path,
    language: Option<&str>,
) -> Option<(std::sync::Arc<crate::translate::Translator>, String)> {
    let settings = match crate::translate::Settings::read(path).await {
        Ok(settings) => settings,
        Err(error) => {
            eprintln!("inocode: {}: {error}", path.display());
            return None;
        }
    };
    if !settings.thinking {
        return None;
    }
    let to = crate::translate::target(&settings, language)?;
    let translator = crate::translate::shared(&settings, path.with_file_name("translation-models"));
    Some((translator, to))
}

impl Thread<'_> {
    /// Records that `rule` was allowed for the call `name`; a failure goes
    /// into the turn's warnings, since the record is the rule's audit.
    async fn record_allowed(&mut self, rule: crate::store::NoteId, actor: Actor, name: &str) {
        if let Err(error) = self.store.record(rule, "allowed", actor, None, name).await {
            self.warnings.push(format!(
                "the approval of rule {} could not be recorded: {}",
                rule.0, error.message
            ));
        }
    }

    /// Starts naming the thread from `prompt` when this is the person's
    /// first message and the thread has no title.
    async fn start_title(
        &mut self,
        llm: &std::sync::Arc<Llm>,
        titling: crate::title::Titling,
        prompt: &str,
    ) {
        if !self.first {
            return;
        }
        match self.store.session_title(self.session).await {
            Ok(title) if title.is_empty() => {}
            Ok(_) => return,
            Err(error) => {
                self.warnings
                    .push(format!("the thread was not named: {}", error.message));
                return;
            }
        }
        let spent = crate::title::Spent::new(std::sync::Mutex::new(Some(TurnUsage::default())));
        crate::title::start(
            self.store.clone(),
            self.session,
            llm.clone(),
            titling,
            prompt.to_string(),
            self.events.clone(),
            spent.clone(),
        );
        self.title_spent = Some(spent);
    }

    /// A `title` call: proposes a title to the person, or sets it.
    async fn title(&mut self, arguments: &str) -> Answer {
        let call = match crate::title::call(arguments) {
            Ok(call) => call,
            Err(message) => return (message, true),
        };
        let current = match self.store.session_title(self.session).await {
            Ok(current) => current,
            Err(error) => return (format!("title failed: {}", error.message), true),
        };
        match call.action {
            crate::title::Action::Propose => {
                if call.title == current {
                    return ("This is already the thread's title.".to_string(), false);
                }
                if !self.proposed.insert(call.title.clone()) {
                    return (
                        "This title was already proposed; it is not shown again. Keep working."
                            .to_string(),
                        false,
                    );
                }
                let _ = self.events.send(TurnEvent::TitleProposed(call.title)).await;
                (
                    "The proposal is shown to the person; the title is unchanged. Keep working."
                        .to_string(),
                    false,
                )
            }
            crate::title::Action::Set => {
                match self.store.rename_session(self.session, &call.title).await {
                    Ok(_) => {}
                    Err(error) => return (format!("title failed: {}", error.message), true),
                }
                let answer = format!("The thread's title is now: {}", call.title);
                let _ = self.events.send(TurnEvent::Title(call.title)).await;
                (answer, false)
            }
        }
    }

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
        let mcp = crate::mcp::render_problems(self.store).await;
        let goal = self
            .goal
            .as_ref()
            .filter(|goal| goal.is_active())
            .map(crate::goal::Goal::render);
        let now = now(chrono::Local::now().fixed_offset());
        let title = self.store.session_title(self.session).await?;
        let title = (!title.is_empty()).then(|| format!("The thread's title: {title}"));
        let plan_mode = self.plan.then_some(crate::plan_mode::STATE);
        let parts: Vec<&str> = [
            Some(now.as_str()),
            title.as_deref(),
            plan_mode,
            self.git.as_deref(),
            goal.as_deref(),
            plan.as_deref(),
            jobs.as_deref(),
            self.lessons.as_deref(),
            skills.as_deref(),
            mcp.as_deref(),
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
            Decision::Ask(rules) => asked_rules = rules,
        }
        if self.plan && plan_refuses(name, arguments) {
            return (crate::plan_mode::REFUSED.to_string(), true);
        }
        if self.mode != Mode::Full || (self.plan && plan_asks(name, arguments)) {
            if let Some(answer) = self
                .approval(llm, pending, name, arguments, asked_rules, usage)
                .await
            {
                return answer;
            }
        }
        if name == crate::plan_mode::EXIT_PLAN && self.plan {
            return self.exit_plan(arguments).await;
        }
        // What MCP servers have the model run during the call counts in the
        // turn's usage.
        let sampled = std::sync::Mutex::new(TurnUsage::default());
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
            sampling: Some(crate::mcp::Sampling {
                model: llm,
                usage: &sampled,
            }),
        };
        if name == crate::goal::GOAL {
            return self.claim(arguments).await;
        }
        if name == crate::title::TITLE {
            return self.title(arguments).await;
        }
        if name == crate::company::REVIEW {
            if let Some(company) = self.company.clone() {
                return crate::company::review(&company, llm, &context, arguments, usage).await;
            }
        }
        if name == crate::explore::EXPLORE {
            let explorer = self.explorer.clone();
            let model = explorer.as_deref().unwrap_or(llm);
            return crate::explore::run(model, &context, arguments, usage)
                .await
                .map_or_else(|message| (message, true), |text| (text, false));
        }
        // A tool that does not watch the turn's cancel signal itself is
        // dropped when the turn is stopped; one that does (shell, MCP) is
        // polled first, so its own answer wins.
        let mut cancel = self.cancel.clone();
        let answer = tokio::select! {
            biased;
            answer = agent_tools::run(&context, name, arguments) => {
                answer.unwrap_or_else(|| (format!("no tool named {name} is available"), true))
            }
            () = cancelled(&mut cancel) => (
                "Stopped with the turn before it finished.".to_string(),
                true,
            ),
        };
        drop(context);
        let sampled = sampled
            .into_inner()
            .unwrap_or_else(|error| error.into_inner());
        usage.requests += sampled.requests;
        usage.input += sampled.input;
        usage.output += sampled.output;
        usage.cached += sampled.cached;
        usage.records.extend(sampled.records);
        answer
    }

    /// Why a call needs approval in this mode, if it does; `asked_rules`
    /// are the `ask` rules it met.
    fn needs(&self, name: &str, arguments: &str, asked_rules: bool) -> Option<Need> {
        let person = |reason: String| {
            Some(Need {
                reason,
                person: true,
            })
        };
        let review = |reason: String| {
            Some(Need {
                reason,
                person: false,
            })
        };
        if self.plan && plan_asks(name, arguments) {
            return person(crate::plan_mode::ASKS.into());
        }
        let command = (name == shell::SHELL)
            .then(|| shell::shell_arguments(arguments))
            .flatten();
        if asked_rules {
            return Some(Need {
                reason: "it falls under a project rule that asks first".into(),
                // The camera, the microphone and plugin installs are never
                // left to the reviewer.
                person: matches!(
                    name,
                    crate::camera::CAMERA | crate::audio::AUDIO | crate::plugins::INSTALL
                ),
            });
        }
        if name == crate::plugins::INSTALL {
            return person("plugins are installed only with the person's approval".into());
        }
        if name == crate::camera::CAMERA && crate::camera::takes_pictures(arguments) {
            return person("photos and videos are taken only with the person's approval".into());
        }
        if name == crate::audio::AUDIO && crate::audio::records(arguments) {
            return person("sound is recorded only with the person's approval".into());
        }
        if self.sandboxed() && command.as_ref().is_some_and(|c| c.outside_sandbox) {
            return review("it asks to run outside the sandbox".into());
        }
        let places =
            agent_tools::access::Places::of(self.store, self.session, self.folder.as_deref());
        if let Some(outside) = agent_tools::access::outside(name, arguments, &places) {
            match self.mode {
                _ if outside.writes || outside.secret => return person(outside.describe()),
                Mode::Ask => return person(outside.describe()),
                Mode::Auto => return review(outside.describe()),
                Mode::OnRequest | Mode::Full => {}
            }
        }
        let reads = command
            .as_ref()
            .is_some_and(|c| shell::reads_only(&c.command));
        // What an MCP tool does is not known here (its annotations are
        // untrusted), so a call to one is treated as a change.
        let mcp = name.starts_with(crate::mcp::PREFIX);
        let types = name == shell::JOB && shell::job_types(arguments);
        let changes = matches!(
            name,
            crate::edit::EDIT | crate::edit::EDITS | crate::edit::WRITE
        ) || (name == shell::SHELL && !reads)
            || mcp
            || types;
        let sends = matches!(
            name,
            crate::fetch::FETCH | crate::web_search::SEARCH | crate::transcribe::TRANSCRIBE
        );
        match self.mode {
            Mode::Ask if changes => person("every change waits for the person in this mode".into()),
            Mode::Ask if sends => {
                person("what is sent out waits for the person in this mode".into())
            }
            Mode::Auto if mcp => review("calls to MCP tools are reviewed in this mode".into()),
            Mode::Auto if name == shell::SHELL && !reads => {
                review("commands that change something are reviewed in this mode".into())
            }
            Mode::Auto if types => {
                review("input to a running command is reviewed in this mode".into())
            }
            Mode::Auto if sends => review("what is sent out is reviewed in this mode".into()),
            _ => None,
        }
    }

    /// Asks the person whether the call may run, or in the `auto` mode has
    /// it reviewed; a call the person allowed before in this thread or its
    /// project runs without asking. `None` lets the call run.
    async fn approval(
        &mut self,
        llm: &Llm,
        pending: &Body,
        name: &str,
        arguments: &str,
        asked_rules: Vec<NoteId>,
        usage: &mut TurnUsage,
    ) -> Option<Answer> {
        let Need { mut reason, person } = self.needs(name, arguments, !asked_rules.is_empty())?;
        if self.allowed_before(name, arguments).await {
            for rule in &asked_rules {
                self.record_allowed(*rule, Actor::Person, name).await;
            }
            return None;
        }
        if name == crate::plugins::INSTALL {
            reason.push_str(&crate::plugins::preview(self.store, arguments).await);
        }
        if self.mode == Mode::Auto && !person {
            if self.denials.0 >= DENIALS_IN_A_ROW || self.denials.1 >= DENIALS_PER_TURN {
                let reason = "the reviewer refused too many calls in this turn".to_string();
                return self.ask_person(name, arguments, asked_rules, reason).await;
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
                        self.record_allowed(*rule, Actor::Harness, name).await;
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
                    self.ask_person(name, arguments, asked_rules, reason).await
                }
            };
        }
        self.ask_person(name, arguments, asked_rules, reason).await
    }

    /// Whether the person allowed calls of `name` with these arguments
    /// from now on, in this thread or in its project. A failed lookup is
    /// reported and the person is asked.
    async fn allowed_before(&mut self, name: &str, arguments: &str) -> bool {
        match self
            .store
            .allowances(self.session, self.project, name)
            .await
        {
            Ok(allowed) => allowed
                .iter()
                .any(|allowed| same_arguments(allowed, arguments)),
            Err(error) => {
                self.warnings.push(format!(
                    "the calls the person allowed before could not be read: {}",
                    error.message
                ));
                false
            }
        }
    }

    /// Asks the person whether the call may run, as an `elicitation` event
    /// answered with `turn/answer`, and waits for the answer for as long as
    /// the turn runs. `None` lets the call run. The `ask` rules it met are
    /// recorded as allowed by the person when it runs.
    async fn ask_person(
        &mut self,
        name: &str,
        arguments: &str,
        rules: Vec<NoteId>,
        reason: String,
    ) -> Option<Answer> {
        let titles = self.titles(&rules).await;
        let question = serde_json::json!({
            "mode": APPROVAL_MODE,
            "tool": name,
            "arguments": arguments,
            "reason": reason,
            "rules": titles,
        });
        let answer = match self.put_to_person(question).await {
            Heard::Answer(answer) => answer,
            Heard::Nobody => {
                return Some((
                    format!(
                        "Not run: it needs the person's approval ({reason}), and nobody could \
                         be asked. Go on with work that does not depend on it."
                    ),
                    true,
                ))
            }
            Heard::Stopped => {
                return Some((
                    "Not run: the turn was stopped before the person answered.".to_string(),
                    true,
                ))
            }
        };
        if answer.get("action").and_then(serde_json::Value::as_str) != Some("accept") {
            return Some((
                "Denied by the person. It was not run. Do not retry it or work around the \
                 refusal; go on with other work, or if this exact action is essential, say so \
                 to the person."
                    .to_string(),
                true,
            ));
        }
        let scope = Scope::of(answer.get("content")).unwrap_or(Scope::Once);
        let project = match scope {
            Scope::Once => None,
            Scope::Thread => Some(None),
            // A thread outside any project keeps the allowance to itself.
            Scope::Project => Some(self.project),
        };
        if let Some(project) = project {
            if let Err(error) = self
                .store
                .allow(self.session, project, name, arguments)
                .await
            {
                self.warnings.push(format!(
                    "the person's approval of later {name} calls could not be saved, so they \
                     will be asked again: {}",
                    error.message
                ));
            }
        }
        for rule in &rules {
            self.record_allowed(*rule, Actor::Person, name).await;
        }
        None
    }

    /// Shows `question` to the person as an `elicitation` event, with its
    /// `id` added, answered with `turn/answer`, and waits for the answer for
    /// as long as the turn runs.
    async fn put_to_person(&mut self, mut question: serde_json::Value) -> Heard {
        let (id, answer) = self.store.wait_elicitation(self.session);
        question["id"] = serde_json::json!(id);
        self.store.describe_elicitation(id, question.clone());
        if self
            .events
            .send(TurnEvent::Elicitation(question))
            .await
            .is_err()
        {
            self.store.forget_elicitation(id);
            return Heard::Nobody;
        }
        let mut cancel = self.cancel.clone();
        let answer = tokio::select! {
            answer = answer => answer.ok(),
            () = cancelled(&mut cancel) => None,
        };
        self.store.forget_elicitation(id);
        let _ = self.events.send(TurnEvent::Answered(id)).await;
        answer.map_or(Heard::Stopped, Heard::Answer)
    }

    /// An `exit_plan` call: shows the plan to the person. Approved, plan
    /// mode ends for the thread and the model implements the plan in the
    /// same turn; declined, plan mode stays and the model gets the
    /// person's feedback.
    async fn exit_plan(&mut self, arguments: &str) -> Answer {
        let plan = match crate::plan_mode::plan(arguments) {
            Ok(plan) => plan,
            Err(message) => return (message, true),
        };
        let question = serde_json::json!({
            "mode": crate::plan_mode::PLAN_MODE,
            "plan": plan,
        });
        let answer = match self.put_to_person(question).await {
            Heard::Answer(answer) => answer,
            Heard::Nobody => {
                return (
                    "The plan could not be shown: nobody could be asked. Plan mode stays on; \
                     end your answer with the plan, so the person can read it."
                        .to_string(),
                    true,
                )
            }
            Heard::Stopped => {
                return (
                    "The turn was stopped before the person answered.".to_string(),
                    true,
                )
            }
        };
        if answer.get("action").and_then(serde_json::Value::as_str) == Some("accept") {
            if let Err(error) = crate::plan_mode::set(self.store, self.session, false).await {
                return (
                    format!(
                        "The person approved the plan, but plan mode could not be turned off \
                         ({}). Tell the person.",
                        error.message
                    ),
                    true,
                );
            }
            self.plan = false;
            let _ = self.events.send(TurnEvent::PlanMode(false)).await;
            return (
                "The person approved the plan. Plan mode is off: implement the plan now."
                    .to_string(),
                false,
            );
        }
        let feedback = answer
            .get("content")
            .and_then(|content| content.get("feedback"))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|feedback| !feedback.is_empty());
        let text = match feedback {
            Some(feedback) => format!(
                "The person did not approve the plan; plan mode stays on. Their feedback:\n\
                 {feedback}\n\
                 Revise the plan, investigating more if needed, and present it again with \
                 exit_plan."
            ),
            None => "The person did not approve the plan and gave no feedback; plan mode stays \
                     on. Revise the plan and present it again with exit_plan, or end your \
                     answer and ask the person what to change."
                .to_string(),
        };
        (text, false)
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
                self.warnings
                    .push(format!("the colleague could not be asked: {error}"));
                Ok(None)
            }
        }
    }

    /// Puts a question to the person: it is added to the thread, so the
    /// model knows and the person can reply to it, and reported when the
    /// turn ends with the questions of `ask`.
    async fn escalate(&mut self, question: &str, options: &[&str]) -> Result<(), ErrorInfo> {
        // A goal runs without asking the person; its limits stop it instead.
        // A member's run cannot ask the person; the lead reads its report.
        if self.goal_active() || self.budget.is_some() {
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
            images: Vec::new(),
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
    async fn record_attempt(&mut self, ending: &Ending) {
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
            self.warnings.push(format!(
                "the unfinished turn was not recorded on its issue: {}",
                error.message
            ));
        }
    }

    fn goal_active(&self) -> bool {
        self.goal.as_ref().is_some_and(crate::goal::Goal::is_active)
    }

    /// Runs the person's command and words its end as the person's
    /// message; `true` when the turn was stopped while it ran.
    async fn run_for_person(
        &mut self,
        command: &str,
        cancel: &watch::Receiver<bool>,
    ) -> Result<(String, bool), ErrorInfo> {
        let place = crate::shell::Place {
            session: self.session,
            folder: self.folder.as_deref(),
            logs: self.store.files_folder().join("logs"),
            cancel: cancel.clone(),
            sandboxed: false,
            activity: self.store.activity(),
        };
        let job = crate::shell::PersonJob::start(place, command)
            .map_err(|message| ErrorInfo::new("shell_failed", message))?;
        let _ = self.events.send(TurnEvent::ShellStarted(job.id())).await;
        let (ended, stopped) = job.finish(cancel.clone()).await;
        let mut text = format!(
            "I ran this command myself in the project folder, outside the sandbox:\n$ {command}\n{ended}"
        );
        if stopped {
            text.push_str("\nI stopped it before it ended.");
        }
        Ok((text, stopped))
    }

    /// Reports each request recorded since the last report.
    async fn report_usage(&mut self, llm: &Llm, usage: &TurnUsage) {
        if self.reported >= usage.records.len() {
            return;
        }
        let window = llm.context_window().await;
        for record in &usage.records[self.reported..] {
            if record.purpose == "turn" {
                if let Some(measured) = record.usage {
                    self.context_used = Some(measured.input);
                }
            }
            let report = UsageReport::new(record, usage, self.context_used, window);
            let _ = self.events.send(TurnEvent::Usage(report)).await;
        }
        self.reported = usage.records.len();
    }

    /// Sends what is left of the task by the estimate, from what it used
    /// so far. Nothing is sent without past tasks to go by.
    async fn send_estimate(&self, usage: &TurnUsage) {
        let Some((forecast, before, started)) = &self.forecast else {
            return;
        };
        if !forecast.has_history() {
            return;
        }
        let used = before + usage.input + usage.output;
        let estimate = forecast.remaining(used, started.elapsed().as_millis() as u64);
        let _ = self.events.send(TurnEvent::Estimate(estimate)).await;
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

    /// After a step of work under an active goal: stops the goal when the
    /// tokens since its last check use up its budget, so a long answer does
    /// not run past it. `true` when the goal was stopped.
    async fn charge_goal(&mut self, usage: &TurnUsage) -> Result<bool, ErrorInfo> {
        let spent = (usage.input + usage.output).saturating_sub(self.goal_mark);
        let Some(mut goal) = crate::goal::load(self.store, self.session)
            .await?
            .filter(crate::goal::Goal::is_active)
        else {
            return Ok(false);
        };
        let Some(text) = crate::goal::charge(&mut goal, spent) else {
            return Ok(false);
        };
        self.goal_mark = usage.input + usage.output;
        crate::goal::save(self.store, self.session, &goal).await?;
        self.goal = Some(goal);
        self.record(vec![Body::User {
            from: Speaker::Harness(crate::goal::GOAL_KIND.to_string()),
            parts: vec![text],
            images: Vec::new(),
        }])
        .await?;
        Ok(true)
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
            sampling: None,
        };
        let checked = crate::goal::check(
            checker.as_deref().unwrap_or(llm),
            &context,
            &goal,
            &work,
            usage,
        )
        .await;
        // Read again: the person may have stopped or replaced the goal while
        // it was checked, and the finding is about the goal as it was.
        let current = crate::goal::load(self.store, self.session).await?;
        match current {
            Some(now) if now.is_active() && now.same_goal(&goal) => goal = now,
            current => {
                self.goal = current;
                self.record(vec![Body::User {
                    from: Speaker::Harness(crate::goal::GOAL_KIND.to_string()),
                    parts: vec![
                        "The goal was stopped or replaced while it was checked; the check's \
                         finding was not applied."
                            .to_string(),
                    ],
                    images: Vec::new(),
                }])
                .await?;
                return Ok(false);
            }
        }
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
            images: Vec::new(),
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

    /// Answers the `delegate` calls among `parts`, by call id, with how
    /// long each took: at the same time when [`crate::company::together`]
    /// allows it, else one after another. Each passes the project's rules
    /// first.
    async fn delegate_all(
        &mut self,
        parts: &[AssistantPart],
        usage: &mut TurnUsage,
    ) -> HashMap<String, (Answer, std::time::Duration)> {
        let calls: Vec<(&str, &str)> = parts
            .iter()
            .filter_map(|part| match part {
                AssistantPart::ToolCall {
                    call_id,
                    name,
                    arguments,
                    ..
                } if name == crate::company::DELEGATE => {
                    Some((call_id.as_str(), arguments.as_str()))
                }
                _ => None,
            })
            .collect();
        let mut answers = HashMap::new();
        if calls.is_empty() {
            return answers;
        }
        let refused = |text: &str| ((text.to_string(), true), std::time::Duration::ZERO);
        if self.plan {
            for (call, _) in calls {
                answers.insert(call.to_string(), refused(crate::plan_mode::REFUSED));
            }
            return answers;
        }
        let company = match (&self.company, self.budget) {
            (_, Some(_)) => {
                for (call, _) in calls {
                    let text = "Not run: a member cannot hand its task on; do it, or say in \
                                your report what is left.";
                    answers.insert(call.to_string(), refused(text));
                }
                return answers;
            }
            (None, None) => {
                for (call, _) in calls {
                    answers.insert(
                        call.to_string(),
                        refused("no tool named delegate is available"),
                    );
                }
                return answers;
            }
            (Some(company), None) => company.clone(),
        };
        let mut runnable = Vec::new();
        for (call, arguments) in calls {
            match self
                .store
                .check(self.project, crate::company::DELEGATE, arguments, None)
                .await
            {
                Ok(Decision::Allow) => runnable.push((call, arguments)),
                Ok(Decision::Block(rules)) => {
                    let titles = self.titles(&rules).await;
                    let text = format!("Blocked by the project rule: {}.", titles.join("; "));
                    answers.insert(call.to_string(), ((text, true), std::time::Duration::ZERO));
                }
                Ok(Decision::Ask(rules)) => {
                    let delegate = crate::company::DELEGATE;
                    if self.allowed_before(delegate, arguments).await {
                        for rule in &rules {
                            self.record_allowed(*rule, Actor::Person, delegate).await;
                        }
                        runnable.push((call, arguments));
                        continue;
                    }
                    let reason = "it falls under a project rule that asks first".to_string();
                    match self.ask_person(delegate, arguments, rules, reason).await {
                        None => runnable.push((call, arguments)),
                        Some(answer) => {
                            answers.insert(call.to_string(), (answer, std::time::Duration::ZERO));
                        }
                    }
                }
                Err(error) => {
                    let text = format!("Not run: the rule check failed ({}).", error.message);
                    answers.insert(call.to_string(), ((text, true), std::time::Duration::ZERO));
                }
            }
        }
        let lead = crate::company::Lead {
            store: self.store,
            session: self.session,
            project: self.project,
            mode: self.mode,
            cancel: self.cancel.clone(),
            reviewer: self.reviewer.clone(),
            compactor: self.compactor.clone(),
            explorer: self.explorer.clone(),
            checker: self.checker.clone(),
            colleague: self.colleague.clone(),
            company,
            request: self.request.clone(),
        };
        let plans: Vec<(bool, Vec<String>)> = runnable
            .iter()
            .map(|(_, arguments)| crate::company::plan(arguments))
            .collect();
        let mut done = Vec::new();
        if crate::company::together(&plans) {
            let started = std::time::Instant::now();
            let runs = runnable
                .iter()
                .map(|(_, arguments)| crate::company::delegate(&lead, arguments));
            let finished = futures_util::future::join_all(runs).await;
            for ((call, _), delegated) in runnable.iter().zip(finished) {
                done.push((call.to_string(), delegated, started.elapsed()));
            }
        } else {
            for (call, arguments) in &runnable {
                let started = std::time::Instant::now();
                let delegated = crate::company::delegate(&lead, arguments).await;
                done.push((call.to_string(), delegated, started.elapsed()));
            }
        }
        for (call, delegated, took) in done {
            usage.requests += delegated.usage.requests;
            usage.input += delegated.usage.input;
            usage.output += delegated.usage.output;
            usage.cached += delegated.usage.cached;
            self.warnings.extend(delegated.warnings);
            answers.insert(call, (delegated.answer, took));
        }
        answers
    }

    /// Results for the calls of an assistant message. Calls run only when
    /// the model stopped to have them run; the calls of an answer cut at
    /// its length limit may be incomplete and are refused.
    async fn results(&mut self, llm: &Llm, body: &Body, usage: &mut TurnUsage) -> Vec<Body> {
        let Body::Assistant { stop, parts, .. } = body else {
            return Vec::new();
        };
        let stop = *stop;
        let mut delegated = match stop {
            StopReason::ToolUse | StopReason::Completed => self.delegate_all(parts, usage).await,
            _ => HashMap::new(),
        };
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
                    let started = std::time::Instant::now();
                    let answer = match delegated.remove(call_id) {
                        Some((answer, took)) => {
                            self.tally.add(name, arguments, took);
                            answer
                        }
                        None => {
                            let answer = self.answer(llm, body, name, arguments, usage).await;
                            self.tally.add(name, arguments, started.elapsed());
                            answer
                        }
                    };
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
        images: Vec::new(),
    }
}

/// Runs one turn of `session`, starting with `prompt`. Input from `steer`
/// joins the thread before the next request. Every message is stored as it
/// is made, so an interrupted or failed turn leaves what happened. The turn
/// works in the mode of `approvals`.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    models: Models,
    store: &Store,
    session: SessionId,
    prompt: Prompt,
    approvals: Approvals,
    mut steer: mpsc::UnboundedReceiver<String>,
    mut cancel: watch::Receiver<bool>,
    events: mpsc::Sender<TurnEvent>,
) -> Outcome {
    let mut usage = TurnUsage::default();
    let llm = models.main.borrow().clone();
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
            thread.reviewer = models.reviewer;
            thread.compactor = models.compactor;
            thread.explorer = models.explorer;
            thread.checker = models.checker.or_else(|| models.colleague.clone());
            thread.colleague = models.colleague;
            if let Some(company) = &models.company {
                thread.warnings.extend(company.warnings.iter().cloned());
            }
            thread.company = models.company.filter(|company| !company.members.is_empty());
            thread.budget = models.budget;
            if let Some(titling) = models.title {
                thread.start_title(&llm, titling, &prompt.text).await;
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
                questions: Vec::new(),
                warnings: Vec::new(),
                edited: Vec::new(),
                over_budget: false,
            }
        }
    };
    let task = TaskStart::begin(&mut thread, &llm, &prompt.text).await;
    drop(llm);
    let mut prompt = prompt;
    let ending = loop {
        let ending = steps(
            models.main.clone(),
            models.mode.clone(),
            &mut thread,
            prompt,
            &mut steer,
            &mut cancel,
            &mut usage,
        )
        .await;
        // Steering accepted while the turn was deciding to end is answered:
        // once the channel is closed no more is accepted, and what came
        // before is the next request's prompt.
        steer.close();
        let mut left = Vec::new();
        while let Ok(text) = steer.try_recv() {
            left.push(text);
        }
        if left.is_empty() {
            break ending;
        }
        if matches!(ending, Ending::Completed | Ending::Length) {
            prompt = Prompt {
                text: left.join("\n\n"),
                ..Prompt::default()
            };
            continue;
        }
        // An interrupted or failed turn keeps the input in the thread, for
        // the next turn to see.
        if let Err(error) = thread.record(left.into_iter().map(person).collect()).await {
            thread.warnings.push(format!(
                "input sent while the turn ended was not kept: {}",
                error.message
            ));
        }
        break ending;
    };
    let wait = match ending {
        Ending::Interrupted => std::time::Duration::ZERO,
        _ => TRANSLATION_WAIT,
    };
    thread.thinking_translation.end(wait).await;
    if ending == Ending::Interrupted {
        shell::stop_all(&store.files_folder().join("logs"), session);
    }
    thread.record_attempt(&ending).await;
    if let Some(spent) = &thread.title_spent {
        crate::title::settle(spent, &mut usage);
    }
    if let Err(error) = store.log_requests(session, &usage.records).await {
        thread.warnings.push(format!(
            "the requests of this turn were not logged, so the statistics leave them out: {}",
            error.message
        ));
    }
    let edited: Vec<String> = thread.tally.edited.iter().cloned().collect();
    task.end(&mut thread, &usage, &ending).await;
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
        questions,
        warnings: thread.warnings,
        edited,
        over_budget: thread.over_budget,
    }
}

/// What the record of a turn as a task needs from its start
/// (docs/evidence/effort-estimates.md).
struct TaskStart {
    started_ms: i64,
    started: std::time::Instant,
    prompt: String,
    provider: String,
    model: String,
    effort: String,
    /// The goal the turn works on, if one is active.
    goal: Option<crate::goal::Goal>,
    snapshot: Option<crate::git::Snapshot>,
}

impl TaskStart {
    /// Notes the start, and prepares and sends the first estimate.
    async fn begin(thread: &mut Thread<'_>, llm: &Llm, prompt: &str) -> Self {
        let goal = thread.goal.clone().filter(crate::goal::Goal::is_active);
        let snapshot = match thread.folder.as_deref() {
            Some(folder) => crate::git::snapshot(folder).await,
            None => None,
        };
        // A goal is one task over its turns: what it used before counts.
        let (request, before) = match &goal {
            Some(goal) => {
                let before = thread
                    .store
                    .goal_task_tokens(thread.session, goal.from_seq)
                    .await;
                (goal.objective.clone(), before)
            }
            None => (prompt.to_string(), Ok(0)),
        };
        let forecast = match before {
            Ok(before) => crate::estimate::forecast(thread.store, &request, llm.model())
                .await
                .map(|forecast| (forecast, before)),
            Err(error) => Err(error),
        };
        match forecast {
            Ok((forecast, before)) => {
                thread.forecast = Some((forecast, before, std::time::Instant::now()));
                thread.send_estimate(&TurnUsage::default()).await;
            }
            Err(error) => thread.warnings.push(format!(
                "no estimate could be made for this turn: {}",
                error.message
            )),
        }
        Self {
            started_ms: crate::store::now(),
            started: std::time::Instant::now(),
            prompt: prompt.to_string(),
            provider: llm.provider_label(),
            model: llm.model().to_string(),
            effort: llm.effort().await.as_str().to_string(),
            goal,
            snapshot,
        }
    }

    /// Records the turn as a task, and adds it to its goal's record.
    async fn end(self, thread: &mut Thread<'_>, usage: &TurnUsage, ending: &Ending) {
        let changed = match (&self.snapshot, thread.folder.as_deref()) {
            (Some(snapshot), Some(folder)) => crate::git::changed_since(folder, snapshot).await,
            _ => None,
        };
        let (lines_added, lines_removed, files_changed, changes_from) = match changed {
            Some(changed) => (
                Some(changed.added),
                Some(changed.removed),
                Some(changed.files),
                "git",
            ),
            None => (None, None, Some(thread.tally.edited.len() as u64), "tools"),
        };
        let task = crate::estimate::Task {
            started_ms: self.started_ms,
            session: Some(thread.session.0),
            kind: "turn".into(),
            goal_seq: self.goal.as_ref().map(|goal| goal.from_seq),
            task_type: crate::estimate::recorded_type(&self.prompt, &thread.tally.counts).into(),
            request: self.prompt,
            provider: self.provider,
            model: self.model,
            effort: Some(self.effort),
            input: usage.input,
            output: usage.output,
            cached: usage.cached,
            requests: usage.requests,
            wall_ms: self.started.elapsed().as_millis() as u64,
            tool_ms: thread.tally.tool_ms,
            tools: thread.tally.counts,
            lines_added,
            lines_removed,
            files_changed,
            changes_from: changes_from.into(),
            outcome: ending.as_str().into(),
        };
        let mut recorded = thread.store.record_task(&task).await;
        if let (Ok(()), Some(goal)) = (&recorded, &self.goal) {
            let outcome = match crate::goal::load(thread.store, thread.session).await {
                Ok(Some(now)) if now.same_goal(goal) => match now.state {
                    crate::goal::State::Active => "running",
                    crate::goal::State::Achieved => "achieved",
                    _ => "abandoned",
                },
                Ok(_) => "abandoned",
                Err(error) => {
                    thread.warnings.push(format!(
                        "the goal's task record was not updated: {}",
                        error.message
                    ));
                    return;
                }
            };
            recorded = thread
                .store
                .add_to_goal_task(&task, goal.from_seq, goal.objective.clone(), outcome)
                .await;
        }
        if let Err(error) = recorded {
            thread.warnings.push(format!(
                "this turn was not recorded as a task, so estimates leave it out: {}",
                error.message
            ));
        }
    }
}

/// Compacts the history of `session` now, as the turn does near the end of
/// `llm`'s context window: the history is treated as filling the window
/// (or its estimated size when larger), so the older part is replaced by a
/// summary and the recent part kept. Returns the stored compaction
/// message's position (`None` when nothing could be replaced) and what went
/// wrong beside it.
/// Who asks a side question, as the model is told.
const ASIDE_KIND: &str = "aside";

/// Answers the person's side question from `session`'s conversation as the
/// model is sent it, without tools, and keeps neither the question nor the
/// answer (docs/evidence/side-question.md). The answer's text comes on
/// `deltas` as it is written.
pub(crate) async fn aside(
    llm: &Llm,
    store: &Store,
    session: SessionId,
    question: &str,
    deltas: mpsc::Sender<String>,
    mut cancel: watch::Receiver<bool>,
) -> Result<String, ErrorInfo> {
    let (events, _) = mpsc::channel(1);
    let thread = open(
        store,
        session,
        "",
        Approvals::default(),
        cancel.clone(),
        events,
    )
    .await?;
    // The thread's own tools, so that the start of the request is the one
    // its turns send and can be read from the provider's cache.
    let tools = agent_tools::specs(thread.sandboxed());
    let mut sent = thread.conversation;
    sent.messages.push(Message {
        id: "aside".to_string(),
        body: Body::User {
            from: Speaker::Harness(ASIDE_KIND.to_string()),
            parts: vec![format!(
                "The person asks a side question while the work may go on. Answer it \
                 from this conversation only, briefly. You cannot use tools for it, so call \
                 none. The question and your answer are not kept in the thread.\n\n\
                 Question: {question}"
            )],
            images: Vec::new(),
        },
    });
    let started = tokio::select! {
        started = llm.converse(&sent, &tools) => started?,
        () = cancelled(&mut cancel) => {
            return Err(ErrorInfo::new("cancelled", "the side question was stopped"));
        }
    };
    let mut stream = started;
    let mut answer = String::new();
    let mut failure = None;
    let mut stopped = false;
    loop {
        tokio::select! {
            next = stream.next_piece() => match next {
                Ok(Some(crate::llm::Piece::Text(text))) => {
                    answer.push_str(&text);
                    let _ = deltas.send(text).await;
                }
                Ok(Some(crate::llm::Piece::Thinking(_))) => {}
                Ok(None) => break,
                Err(error) => {
                    failure = Some(error);
                    break;
                }
            },
            () = cancelled(&mut cancel) => {
                stopped = true;
                break;
            }
        }
    }
    let _ = store
        .log_requests(session, &[stream.record(ASIDE_KIND)])
        .await;
    if let Some(error) = failure {
        return Err(error);
    }
    if stopped {
        return Err(ErrorInfo::new("cancelled", "the side question was stopped"));
    }
    Ok(answer)
}

pub(crate) async fn compact_now(
    llm: &Llm,
    compactor: Option<&Llm>,
    store: &Store,
    session: SessionId,
) -> Result<(Option<i64>, Option<String>), ErrorInfo> {
    let window = llm.context_window().await.ok_or_else(|| {
        ErrorInfo::new(
            "unavailable",
            "the model's context window is not known, so what to keep cannot be chosen",
        )
    })?;
    let (events, _) = mpsc::channel(1);
    let (_keep, cancel) = watch::channel(false);
    let mut thread = open(store, session, "", Approvals::default(), cancel, events).await?;
    let tools = agent_tools::specs(thread.sandboxed());
    let bytes = compaction::request_bytes(
        thread.conversation.system.as_deref(),
        &thread.conversation.messages,
        &tools,
    );
    let ratio = llm
        .bytes_per_token(session.0)
        .unwrap_or(compaction::FALLBACK_BYTES_PER_TOKEN);
    let input = compaction::tokens(bytes, ratio).max(window);
    let sent: Vec<(i64, Message)> = thread
        .seqs
        .iter()
        .copied()
        .zip(thread.conversation.messages.iter().cloned())
        .collect();
    let compacted = compaction::compact_reporting(
        llm,
        compactor,
        thread.conversation.system.clone(),
        &sent,
        &tools,
        input,
        window,
    )
    .await;
    let Some((body, records, failure)) = compacted else {
        return Ok((None, None));
    };
    let logged = store.log_requests(session, &records).await;
    // Asked for by the person, a compaction without a summary is a failure
    // and leaves the history as it was.
    if let Some(why) = failure {
        return Err(ErrorInfo::new(
            "compaction_failed",
            format!("no summary could be written ({why}); the thread was not compacted"),
        ));
    }
    thread.record(vec![body]).await?;
    let warning = logged.err().map(|error| {
        format!(
            "the summary requests were not logged, so the statistics leave them out: {}",
            error.message
        )
    });
    Ok((thread.seqs.last().copied(), warning))
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
    let first = !stored
        .iter()
        .any(|stored| crate::promise::person_text(&stored.message).is_some());
    let proposed = crate::title::proposed(stored.iter().map(|stored| &stored.message))
        .into_iter()
        .collect();
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
    let thinking_translation = ThinkingTranslation::read(
        store.files_folder().with_file_name("settings.json"),
        language.clone(),
        events.clone(),
    )
    .await;
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
        asked: Vec::new(),
        failures: HashMap::new(),
        lessons,
        language,
        mode: approvals.mode,
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
        warnings: Vec::new(),
        git,
        thinking_translation,
        tally: Default::default(),
        forecast: None,
        company: None,
        budget: None,
        over_budget: false,
        first,
        proposed,
        title_spent: None,
        plan: false,
        reported: 0,
        context_used: None,
    })
}

/// Adds the company's tools. A member's run (`delegated`) reports to the
/// lead instead of asking the person, and cannot hand its task on: one
/// level of delegation only, so that tasks are not passed down a chain.
fn company_tools(
    harness: &mut Vec<ToolSpec>,
    company: Option<&crate::company::Company>,
    delegated: bool,
) {
    if delegated {
        harness.retain(|tool| tool.name != crate::ask::ASK);
    }
    if let Some(company) = company {
        if !delegated {
            harness.push(crate::company::delegate_spec(company));
        }
        harness.push(crate::company::review_spec(company));
    }
}

/// Whether a member's run with `budget` stops after a step that would go
/// on: its tokens, input and output, reached the budget.
fn over_budget(budget: Option<u64>, usage: &TurnUsage, step: Option<&Step>) -> bool {
    let going_on = matches!(step, Some(Step::Continue | Step::Resume));
    budget.is_some_and(|budget| going_on && usage.input + usage.output >= budget)
}

async fn steps(
    main: watch::Receiver<std::sync::Arc<Llm>>,
    mut mode: watch::Receiver<Option<Mode>>,
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
    company_tools(
        &mut harness,
        thread.company.as_deref(),
        thread.budget.is_some(),
    );
    // A member's run works on a task of the lead, not with the person.
    if thread.budget.is_none() {
        harness.push(crate::title::spec());
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
    let mut prompt = prompt;
    if let Some(command) = prompt.shell.take() {
        match thread.run_for_person(&command, cancel).await {
            Ok((text, false)) => prompt.text = text,
            Ok((text, true)) => {
                if let Err(error) = thread.record(vec![person(text)]).await {
                    return Ending::Failed(error);
                }
                return Ending::Interrupted;
            }
            Err(error) => return Ending::Failed(error),
        }
    }
    thread.request = prompt.text.clone();
    thread.advised_from = thread.conversation.messages.len();
    let images = thread
        .store
        .files_folder()
        .join("images")
        .join(format!("thread-{}", thread.session.0));
    match crate::attach::message(prompt.text, prompt.attached, images).await {
        Ok(message) => opening.push(message),
        Err(error) => return Ending::Failed(error),
    }
    if let Err(error) = thread.record(opening).await {
        return Ending::Failed(error);
    }
    if let Err(error) = thread.check_issue(cancel, usage).await {
        return Ending::Failed(error);
    }
    let mut cuts = 0;
    loop {
        if mode.has_changed().unwrap_or(false) {
            if let Some(chosen) = *mode.borrow_and_update() {
                thread.mode = chosen;
            }
        }
        match crate::plan_mode::on(thread.store, thread.session).await {
            Ok(on) => thread.plan = on,
            Err(error) => return Ending::Failed(error),
        }
        let llm = main.borrow().clone();
        let llm = llm.as_ref();
        // MCP tools found by a search are offered from the next request on.
        let mut tools = harness.clone();
        if thread.plan {
            tools.push(crate::plan_mode::spec());
        }
        tools.extend(crate::mcp::specs(thread.store, &thread.conversation.messages).await);
        let compacted = tokio::select! {
            compacted = thread.compact_if_due(llm, &tools, usage) => Some(compacted),
            () = cancelled(cancel) => None,
        };
        match compacted {
            Some(Ok(())) => thread.report_usage(llm, usage).await,
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
                    images: Vec::new(),
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
        thread.thinking_translation.refresh().await;
        loop {
            tokio::select! {
                next = stream.next_piece() => match next {
                    Ok(Some(crate::llm::Piece::Text(text))) => {
                        let _ = thread.events.send(TurnEvent::Delta(text)).await;
                    }
                    Ok(Some(crate::llm::Piece::Thinking(text))) => {
                        // Turned on or off while the model thinks: from
                        // this piece on.
                        thread.thinking_translation.refresh().await;
                        thread.thinking_translation.push(&text);
                        let _ = thread.events.send(TurnEvent::Thinking(text)).await;
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
        thread.thinking_translation.answer_ended();
        usage.add_stream(&stream, "turn");
        thread.report_usage(llm, usage).await;
        thread.send_estimate(usage).await;
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
        thread.report_usage(llm, usage).await;
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
                images: Vec::new(),
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
        if over_budget(thread.budget, usage, step.as_ref()) {
            thread.over_budget = true;
            let note = Body::User {
                from: Speaker::Harness(BUDGET_KIND.to_string()),
                parts: vec![format!(
                    "Stopped: this delegated task used up its budget of {} tokens.",
                    thread.budget.unwrap_or_default()
                )],
                images: Vec::new(),
            };
            if let Err(error) = thread.record(vec![note]).await {
                return Ending::Failed(error);
            }
            return Ending::Interrupted;
        }
        match step {
            None => return Ending::Failed(failure.expect("no step only after a failure")),
            Some(Step::Continue) => {
                cuts = 0;
                if thread.goal_active() {
                    match thread.charge_goal(usage).await {
                        Ok(true) => return Ending::Completed,
                        Ok(false) => {}
                        Err(error) => return Ending::Failed(error),
                    }
                }
            }
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
    fn an_approved_call_matches_its_arguments_written_another_way() {
        assert!(same_arguments(
            r#"{"path":"a","content":"x"}"#,
            "{\n  \"content\": \"x\",\n  \"path\": \"a\"\n}"
        ));
        assert!(!same_arguments(
            r#"{"path":"a","content":"x"}"#,
            r#"{"path":"a","content":"y"}"#
        ));
        assert!(same_arguments("not json", "not json"));
        assert!(!same_arguments("not json", "not  json"));
    }

    #[test]
    fn plan_mode_refuses_changes_and_asks_before_what_may_change() {
        for name in [
            crate::edit::EDIT,
            crate::edit::EDITS,
            crate::edit::WRITE,
            crate::plugins::INSTALL,
            crate::company::DELEGATE,
        ] {
            assert!(plan_refuses(name, "{}"), "{name}");
        }
        assert!(!plan_refuses(crate::read::READ, r#"{"path":"a"}"#));
        assert!(!plan_refuses(shell::SHELL, r#"{"command":"mkdir x"}"#));
        assert!(plan_asks(shell::SHELL, r#"{"command":"mkdir x"}"#));
        assert!(!plan_asks(shell::SHELL, r#"{"command":"ls"}"#));
        assert!(plan_asks(
            shell::SHELL,
            r#"{"command":"ls","outside_sandbox":true}"#
        ));
        assert!(plan_asks("mcp__docs__search", "{}"));
        assert!(!plan_asks(crate::read::READ, r#"{"path":"a"}"#));
    }

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

    #[tokio::test]
    async fn a_change_of_the_settings_is_seen_by_a_running_turn() {
        let folder =
            std::env::temp_dir().join(format!("inocode-turn-translation-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        let settings = folder.join("settings.json");
        std::fs::write(&settings, r#"{"translation": {"thinking": false}}"#).unwrap();
        let (events, _received) = mpsc::channel(16);
        let mut translation =
            ThinkingTranslation::read(settings.clone(), Some("ja-JP".into()), events).await;
        assert!(translation.current.is_none());
        crate::translate::update(&settings, &serde_json::json!({"thinking": true}))
            .await
            .unwrap();
        assert!(translation.refresh().await);
        assert_eq!(
            translation.current.as_ref().map(|(_, to)| to.as_str()),
            Some("ja")
        );
        // A write that leaves the translation as it is changes nothing.
        crate::translate::update(&settings, &serde_json::json!({"thinking": true}))
            .await
            .unwrap();
        crate::settings::update(&settings, |file| {
            file.insert("language".into(), serde_json::json!("ja-JP"));
            Ok::<_, String>(())
        })
        .await
        .unwrap();
        assert!(!translation.refresh().await);
        crate::translate::update(&settings, &serde_json::json!({"thinking": false}))
            .await
            .unwrap();
        assert!(translation.refresh().await);
        assert!(translation.current.is_none());
        std::fs::remove_dir_all(&folder).unwrap();
    }

    /// The thinking pushed before the translation was changed is still
    /// sent, and all of it comes before the turn's end, which does not wait
    /// longer than it is given.
    #[tokio::test]
    async fn thinking_waiting_for_translation_is_sent_by_the_end_of_the_turn() {
        let folder = tempfile::tempdir().unwrap();
        let settings = folder.path().join("settings.json");
        // A Google that takes the connection and never answers.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        std::fs::write(
            &settings,
            r#"{"translation": {"thinking": true, "service": "edge", "to": "ja"}}"#,
        )
        .unwrap();
        let (events, mut received) = mpsc::channel(64);
        let mut translation = ThinkingTranslation::read(settings.clone(), None, events).await;
        translation.current = translation.current.take().map(|(_, to)| {
            let stalled = crate::translate::Translator::new(
                crate::translate::Service::Edge,
                None,
                crate::translate::Hosts {
                    edge: format!("http://{}/translate", listener.local_addr().unwrap()),
                    ..crate::translate::Hosts::default()
                },
                folder.path().join("translation-models"),
            );
            (std::sync::Arc::new(stalled), to)
        });
        translation.push("First thought. Unfinished");
        translation.answer_ended();
        crate::translate::update(&settings, &serde_json::json!({"thinking": false}))
            .await
            .unwrap();
        assert!(translation.refresh().await);
        translation.push("Not translated any more. ");
        let started = std::time::Instant::now();
        translation.end(std::time::Duration::from_millis(200)).await;
        assert!(started.elapsed() < std::time::Duration::from_secs(15));
        let mut pieces = Vec::new();
        while let Ok(TurnEvent::ThinkingTranslation(piece)) = received.try_recv() {
            pieces.push(piece);
        }
        let texts: Vec<&str> = pieces.iter().map(|piece| piece.text.as_str()).collect();
        assert_eq!(texts, ["First thought.", " Unfinished"]);
        assert!(pieces.iter().all(|piece| piece.error.is_some()));
    }

    fn company() -> crate::company::Company {
        let llm = Llm::new(crate::LlmConfig {
            provider: crate::ProviderKind::OpenAi,
            auth: crate::oauth::AuthKind::ApiKey,
            base_url: "http://127.0.0.1:9".into(),
            model: "member-model".into(),
            api_key: "key".into(),
            auth_store_path: None,
            reasoning: Default::default(),
        })
        .unwrap();
        crate::company::Company {
            members: vec![crate::company::Member {
                profile: crate::company::Profile {
                    name: "member".into(),
                    role: "implements simple logic".into(),
                    provider: "p".into(),
                    model: "member-model".into(),
                    vendor: "p".into(),
                    price: None,
                    quality: Vec::new(),
                    tokens_per_second: None,
                },
                effort: None,
                llm: std::sync::Arc::new(llm),
            }],
            max_budget: 1_000,
            warnings: Vec::new(),
        }
    }

    fn names(tools: &[ToolSpec]) -> Vec<&str> {
        tools.iter().map(|tool| tool.name.as_str()).collect()
    }

    #[test]
    fn a_members_run_can_review_but_not_delegate_or_ask_the_person() {
        let company = company();
        let mut lead = agent_tools::specs(false);
        company_tools(&mut lead, Some(&company), false);
        let lead = names(&lead);
        assert!(lead.contains(&"delegate") && lead.contains(&"review") && lead.contains(&"ask"));
        let mut member = agent_tools::specs(false);
        company_tools(&mut member, Some(&company), true);
        let member = names(&member);
        assert!(member.contains(&"review"), "{member:?}");
        assert!(
            !member.contains(&"delegate") && !member.contains(&"ask"),
            "{member:?}"
        );
        assert!(
            member.contains(&"edit") && member.contains(&"shell"),
            "{member:?}"
        );
        let mut alone = agent_tools::specs(false);
        company_tools(&mut alone, None, false);
        assert!(!names(&alone).contains(&"delegate"));
        let description = crate::company::delegate_spec(&company).description;
        assert!(description.contains("- member | implements simple logic | p/member-model"));
    }

    #[test]
    fn a_members_run_stops_once_its_tokens_reach_the_budget() {
        let usage = TurnUsage {
            input: 700,
            output: 300,
            ..Default::default()
        };
        assert!(over_budget(Some(1_000), &usage, Some(&Step::Continue)));
        assert!(over_budget(Some(1_000), &usage, Some(&Step::Resume)));
        assert!(!over_budget(Some(1_001), &usage, Some(&Step::Continue)));
        // An answer that ends the run is kept; a run without a budget goes on.
        assert!(!over_budget(
            Some(10),
            &usage,
            Some(&Step::Finish(Ending::Completed))
        ));
        assert!(!over_budget(None, &usage, Some(&Step::Continue)));
    }

    #[test]
    fn the_reply_language_comes_before_the_rules() {
        let system = system_prompt(Some("- rule"), Some("ja-JP"), None);
        let language = system.find("in ja-JP").unwrap();
        assert!(language < system.find("----- RULES -----").unwrap());
        assert!(!system_prompt(None, None, None).contains("Write your replies"));
    }
}
