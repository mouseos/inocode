//! Slash commands: the built-in ones, and prompt commands from installed
//! plugins (their commands and skills), the user and the project. The
//! engine lists them (`commands/list`) and runs what the person types
//! (`command/run`). Decisions: docs/evidence/slash-commands.md.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};

use crate::conversation::{AssistantPart, Body, Message, Speaker};
use crate::store::{HistoryQuery, NewNote, Note, NoteId, NoteKind, NoteQuery, ProjectId};
use crate::store::{SessionId, Source as NoteSource, ThreadSummary};
use crate::{turn, Activity, Choice, Core, ErrorInfo, ThreadChange};

/// When a command may be used while a turn of its thread runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum When {
    /// At once, also during a turn.
    Now,
    /// During a turn too; the running turn uses it from its next request
    /// to the model.
    NextRequest,
    /// Only when no turn runs.
    AfterTurn,
}

/// What running a command gives back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Runs {
    /// One result.
    Result,
    /// A turn, whose events stream like `turn/start`'s.
    Turn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Free text; as the last argument it takes the rest of the line.
    Text,
    /// A file; as the last argument it takes the rest of the line.
    Path,
    /// A whole number, such as an id.
    Number,
    Choice(&'static [&'static str]),
}

#[derive(Debug)]
pub struct Arg {
    pub name: &'static str,
    pub kind: Kind,
    pub required: bool,
}

/// One way of calling a command: its own form when `sub` is `None`, else
/// the subcommand `sub`.
#[derive(Debug)]
pub struct Form {
    pub sub: Option<&'static str>,
    pub description: &'static str,
    pub args: &'static [Arg],
    pub when: When,
    pub runs: Runs,
    /// Why it cannot be used yet: the feature it needs does not exist.
    pub unavailable: Option<&'static str>,
}

#[derive(Debug)]
pub struct Builtin {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub description: &'static str,
    pub forms: &'static [Form],
    /// The subcommand used when nothing follows the name.
    pub bare: Option<&'static str>,
    pub unavailable: Option<&'static str>,
}

const fn arg(name: &'static str, kind: Kind) -> Arg {
    Arg {
        name,
        kind,
        required: true,
    }
}

const fn optional(name: &'static str, kind: Kind) -> Arg {
    Arg {
        name,
        kind,
        required: false,
    }
}

const fn form(description: &'static str, args: &'static [Arg], when: When) -> Form {
    Form {
        sub: None,
        description,
        args,
        when,
        runs: Runs::Result,
        unavailable: None,
    }
}

const fn sub(
    name: &'static str,
    description: &'static str,
    args: &'static [Arg],
    when: When,
) -> Form {
    Form {
        sub: Some(name),
        description,
        args,
        when,
        runs: Runs::Result,
        unavailable: None,
    }
}

const fn turn_form(description: &'static str, args: &'static [Arg]) -> Form {
    Form {
        sub: None,
        description,
        args,
        when: When::AfterTurn,
        runs: Runs::Turn,
        unavailable: None,
    }
}

const fn builtin(
    name: &'static str,
    description: &'static str,
    forms: &'static [Form],
    bare: Option<&'static str>,
) -> Builtin {
    Builtin {
        name,
        aliases: &[],
        description,
        forms,
        bare,
        unavailable: None,
    }
}

const fn missing(name: &'static str, description: &'static str, reason: &'static str) -> Builtin {
    Builtin {
        name,
        aliases: &[],
        description,
        forms: &[],
        bare: None,
        unavailable: Some(reason),
    }
}

use Kind::{Choice as Of, Number, Path as File, Text};
use When::{AfterTurn, NextRequest, Now};

const NUMBER: &[Arg] = &[arg("number", Number)];
const ACCOUNT: &[Arg] = &[arg("provider", Text), arg("account", Text)];
const MODES: &[&str] = &["ask", "on-request", "auto", "full"];
const EFFORTS: &[&str] = &[
    "none", "minimal", "low", "medium", "high", "xhigh", "max", "default",
];
const ROLES: &[&str] = &["review", "compact", "explore", "check", "colleague"];
const ON_OFF: &[&str] = &["on", "off"];
const STRATEGIES: &[&str] = &["quota", "round-robin", "fill-first", "reset-first"];
const SERVICES: &[&str] = &["google", "edge", "firefox", "chromium"];
const SPEECH: &[&str] = &["claude", "chatgpt", "google"];

/// The built-in commands. Their names and aliases cannot be taken by any
/// other command.
pub const BUILTINS: &[Builtin] = &[
    builtin(
        "help",
        "List the commands, or show how to use one.",
        &[form(
            "The commands, or one command.",
            &[optional("command", Text)],
            Now,
        )],
        None,
    ),
    builtin(
        "new",
        "Start a new thread in this thread's project.",
        &[form("Start a new thread.", &[], AfterTurn)],
        None,
    ),
    builtin(
        "resume",
        "List the threads, or pick one to continue.",
        &[form(
            "The threads, the latest first, or the one numbered.",
            &[optional("thread", Number)],
            AfterTurn,
        )],
        None,
    ),
    builtin(
        "rename",
        "Rename this thread.",
        &[form("Give the thread a title.", &[arg("title", Text)], Now)],
        None,
    ),
    builtin(
        "cd",
        "Move this thread to another folder, named by its absolute path; the next turn works there.",
        &[form("Move the thread.", &[arg("folder", Text)], AfterTurn)],
        None,
    ),
    builtin(
        "worktree",
        "Give this thread a git worktree of its project on a new branch, and work there.",
        &[form(
            "Make the worktree.",
            &[optional("branch", Text)],
            AfterTurn,
        )],
        None,
    ),
    builtin(
        "fork",
        "Copy this thread into a new one.",
        &[form(
            "A new thread with this thread's messages, plan, standing instructions and model.",
            &[],
            AfterTurn,
        )],
        None,
    ),
    builtin(
        "delete",
        "Delete this thread for good.",
        &[form(
            "Delete the thread; it cannot be undone.",
            &[arg("confirm", Of(&["confirm"]))],
            AfterTurn,
        )],
        None,
    ),
    builtin(
        "compact",
        "Replace the older part of this thread with a summary now, keeping what the instructions name above all.",
        &[form(
            "Compact the thread.",
            &[optional("instructions", Text)],
            AfterTurn,
        )],
        None,
    ),
    builtin(
        "stop",
        "Interrupt this thread's running turn, from any connection.",
        &[form("Interrupt the running turn.", &[], Now)],
        None,
    ),
    builtin(
        "export",
        "Write this thread out as Markdown or JSON.",
        &[form(
            "The thread as text, or written to the file given.",
            &[
                optional("format", Of(&["markdown", "json"])),
                optional("path", File),
            ],
            Now,
        )],
        None,
    ),
    builtin(
        "history",
        "Search every thread's messages.",
        &[form("Messages matching the words.", &[arg("query", Text)], Now)],
        None,
    ),
    builtin(
        "goal",
        "Set a goal and keep working until a separate checker finds it achieved.",
        &[
            turn_form(
                "Set the goal and start working on it; --check \"command\", --rounds n and \
                 --tokens n before the objective set its check and limits.",
                &[arg("objective", Text)],
            ),
            sub(
                "status",
                "The goal, its progress, the checker's last finding, and the time and tokens it took.",
                &[],
                Now,
            ),
            sub("stop", "Stop the goal.", &[], Now),
        ],
        Some("status"),
    ),
    builtin(
        "plan",
        "Show the plan the agent keeps, or turn plan mode on or off.",
        &[
            form("The plan.", &[], Now),
            sub(
                "on",
                "Plan mode: the agent investigates without changing files and presents a \
                 plan for approval.",
                &[],
                NextRequest,
            ),
            sub("off", "End plan mode.", &[], NextRequest),
        ],
        None,
    ),
    builtin(
        "promise",
        "Show the person's standing instructions the agent keeps.",
        &[
            form("The standing instructions.", &[], Now),
            sub("remove", "Remove the instruction numbered.", NUMBER, Now),
            sub("clear", "Remove every instruction.", &[], Now),
        ],
        None,
    ),
    builtin(
        "issues",
        "The project's unfinished issues.",
        &[
            sub("list", "The unfinished issues.", &[], Now),
            sub(
                "done",
                "Close the issue as done.",
                &[arg("number", Number), optional("reason", Text)],
                Now,
            ),
            sub(
                "drop",
                "Close the issue as not to be done.",
                &[arg("number", Number), optional("reason", Text)],
                Now,
            ),
        ],
        Some("list"),
    ),
    builtin(
        "notes",
        "The lessons and methods the agent recorded.",
        &[
            sub("list", "The lessons and methods.", &[], Now),
            sub("search", "Search them.", &[arg("query", Text)], Now),
            sub(
                "show",
                "One note: its text, tags, links and votes.",
                NUMBER,
                Now,
            ),
            sub(
                "edit",
                "Replace a note's text; the old one is kept as history.",
                &[arg("number", Number), arg("text", Text)],
                Now,
            ),
            sub(
                "tag",
                "Add tags to a note.",
                &[arg("number", Number), arg("tags", Text)],
                Now,
            ),
            sub(
                "vote",
                "Record whether a note helped.",
                &[arg("number", Number), arg("vote", Of(&["up", "down"]))],
                Now,
            ),
            sub(
                "retire",
                "Mark a note as no longer used; it is kept.",
                NUMBER,
                Now,
            ),
        ],
        Some("list"),
    ),
    builtin(
        "rules",
        "The project's rules.",
        &[
            sub("list", "The rules.", &[], Now),
            sub(
                "import",
                "Read the project's AGENTS.md into rules.",
                &[],
                AfterTurn,
            ),
            Form {
                sub: Some("edit"),
                description: "Change a rule's text.",
                args: &[arg("number", Number), arg("text", Text)],
                when: Now,
                runs: Runs::Result,
                unavailable: Some(
                    "a rule's text cannot be changed yet: the store has no way to edit a rule \
                     that keeps its enforcement",
                ),
            },
            sub("off", "Stop using a rule; it is kept.", NUMBER, Now),
        ],
        Some("list"),
    ),
    builtin(
        "colleague",
        "The colleague's settings and its advice in this thread.",
        &[form("The colleague.", &[], Now)],
        None,
    ),
    Builtin {
        name: "members",
        aliases: &["profiles"],
        description: "The members, models with a role, the agent may hand tasks to or ask for \
                      review; adding, changing and removing them.",
        forms: &[
            sub(
                "list",
                "The members, the ones allowed in this project, and the tasks this thread \
                 handed to them.",
                &[],
                Now,
            ),
            sub(
                "add",
                "Add a member: a name, provider/model (or a model of this thread's \
                 provider), and its role in words.",
                &[
                    arg("name", Text),
                    arg("model", Text),
                    optional("role", Text),
                ],
                Now,
            ),
            sub(
                "set",
                "Change a member's provider, model, effort (default removes it) or role.",
                &[
                    arg("name", Text),
                    arg("field", Of(&["provider", "model", "effort", "role"])),
                    arg("value", Text),
                ],
                Now,
            ),
            sub("remove", "Remove a member.", &[arg("name", Text)], Now),
        ],
        bare: Some("list"),
        unavailable: None,
    },
    builtin(
        "allow",
        "Which members and models may be called: the default list, this project's list and \
         the repository's .inocode/models.json.",
        &[
            sub(
                "show",
                "The lists, the repository file's state, and the list in effect here.",
                &[],
                Now,
            ),
            sub(
                "default",
                "Set the default list: member names or provider/model, separated by spaces or \
                 commas; none empties it.",
                &[arg("entries", Text)],
                Now,
            ),
            sub(
                "project",
                "Set this project's list, which replaces the default here; default removes it.",
                &[arg("entries", Text)],
                Now,
            ),
            sub(
                "approve",
                "Use the repository's .inocode/models.json as it is now; a changed file asks \
                 again.",
                &[],
                Now,
            ),
        ],
        Some("show"),
    ),
    builtin(
        "mode",
        "Show or change the approval mode.",
        &[form(
            "The mode, or the mode to use.",
            &[optional("mode", Of(MODES))],
            NextRequest,
        )],
        None,
    ),
    builtin(
        "answer",
        "The questions waiting for the person, or answer one.",
        &[form(
            "The questions, or the answer to the one numbered.",
            &[optional("number", Number), optional("answer", Text)],
            Now,
        )],
        None,
    ),
    builtin(
        "sandbox",
        "Whether commands are confined, and where they may write.",
        &[form("The sandbox.", &[], Now)],
        None,
    ),
    builtin(
        "model",
        "Show or change this thread's provider and model.",
        &[form(
            "The model, or provider/model, model, or default.",
            &[optional("model", Text)],
            NextRequest,
        )],
        None,
    ),
    builtin(
        "effort",
        "Show or change how hard the model thinks.",
        &[form(
            "The effort, or the effort to use.",
            &[optional("effort", Of(EFFORTS))],
            NextRequest,
        )],
        None,
    ),
    builtin(
        "roles",
        "The models of the helper roles, or change one.",
        &[form(
            "The roles, or a role and its provider/model, model, or default.",
            &[optional("role", Of(ROLES)), optional("model", Text)],
            Now,
        )],
        None,
    ),
    builtin(
        "providers",
        "The providers, and adding, changing and removing them.",
        &[
            sub("list", "The providers.", &[], Now),
            sub(
                "add",
                "Add a provider of a kind.",
                &[arg("id", Text), arg("kind", Text)],
                Now,
            ),
            sub(
                "set",
                "Change one field of a provider.",
                &[arg("id", Text), arg("field", Text), arg("value", Text)],
                Now,
            ),
            sub("remove", "Remove a provider.", &[arg("id", Text)], Now),
        ],
        Some("list"),
    ),
    builtin(
        "models",
        "The models with prices, scores and measured speed.",
        &[form(
            "Every provider's models, or one provider's.",
            &[optional("provider", Text)],
            Now,
        )],
        None,
    ),
    builtin(
        "login",
        "Sign in to a provider; a second account of the same provider is added beside the first.",
        &[
            form(
                "Start signing in to the provider (this thread's when none is given) and show \
                 the page to open or the code to enter; code is for Claude when no browser \
                 can return to this machine.",
                &[
                    optional("provider", Text),
                    optional("mode", Of(&["callback", "code", "device"])),
                ],
                Now,
            ),
            sub(
                "code",
                "Give a sign-in the code the provider showed.",
                &[arg("login", Text), arg("code", Text)],
                Now,
            ),
            sub(
                "status",
                "Whether a sign-in finished.",
                &[arg("login", Text)],
                Now,
            ),
            sub("cancel", "Stop a sign-in.", &[arg("login", Text)], Now),
        ],
        None,
    ),
    builtin(
        "logout",
        "Sign out: remove an account of this thread's provider and its stored secret.",
        &[form(
            "Remove the account named, or the one this thread's provider uses.",
            &[optional("account", Text)],
            Now,
        )],
        None,
    ),
    builtin(
        "accounts",
        "The accounts of the sign-in providers: which one is used, pausing, naming and removing them, and the pool that chooses among them.",
        &[
            sub(
                "list",
                "Every provider's accounts, or one provider's.",
                &[optional("provider", Text)],
                Now,
            ),
            sub(
                "use",
                "Make the provider prefer the account from its next request on.",
                ACCOUNT,
                Now,
            ),
            sub("pause", "Stop using the account, keeping it.", ACCOUNT, Now),
            sub("resume", "Use a paused account again.", ACCOUNT, Now),
            sub(
                "rename",
                "Give the account an alias.",
                &[
                    arg("provider", Text),
                    arg("account", Text),
                    arg("alias", Text),
                ],
                Now,
            ),
            sub(
                "remove",
                "Remove the account and its stored secret.",
                ACCOUNT,
                Now,
            ),
            sub(
                "priority",
                "Set the account's pool tier, -100 to 100; higher tiers are used first.",
                &[
                    arg("provider", Text),
                    arg("account", Text),
                    arg("priority", Number),
                ],
                Now,
            ),
            sub(
                "clear-cooldown",
                "Let the pool use the account again after it refused on quota.",
                ACCOUNT,
                Now,
            ),
            sub(
                "strategy",
                "How the provider's pool places new conversations on its accounts.",
                &[arg("provider", Text), arg("strategy", Of(STRATEGIES))],
                Now,
            ),
            sub(
                "status",
                "The pool: each account's usage, cooldown and threads.",
                &[optional("provider", Text)],
                Now,
            ),
        ],
        Some("list"),
    ),
    builtin(
        "usage",
        "The default account's usage and limits.",
        &[form("The usage.", &[], Now)],
        None,
    ),
    builtin(
        "stats",
        "Measured speed and tokens per model.",
        &[form(
            "The last 30 days, or the days given.",
            &[optional("days", Number)],
            Now,
        )],
        None,
    ),
    builtin(
        "status",
        "This thread's model, mode, goal and tokens used.",
        &[form("The status.", &[], Now)],
        None,
    ),
    builtin(
        "estimate",
        "Estimate the time and tokens of a piece of work.",
        &[form(
            "The range from similar past tasks on this thread's model.",
            &[arg("work", Text)],
            Now,
        )],
        None,
    ),
    builtin(
        "translate",
        "The translation shown on screen.",
        &[
            form("The translation settings.", &[], Now),
            sub(
                "thinking",
                "Translate the model's thinking.",
                &[arg("on", Of(ON_OFF))],
                Now,
            ),
            sub(
                "answer",
                "Translate the model's answers (stored only; not implemented yet).",
                &[arg("on", Of(ON_OFF))],
                Now,
            ),
            sub(
                "service",
                "The translation service.",
                &[arg("service", Of(SERVICES))],
                Now,
            ),
            sub(
                "to",
                "The language to translate into.",
                &[arg("language", Text)],
                Now,
            ),
            sub(
                "models",
                "The translation models kept on disk, or remove one.",
                &[
                    optional("remove", Of(&["remove"])),
                    optional("service", Of(SERVICES)),
                    optional("from", Text),
                    optional("to", Text),
                ],
                Now,
            ),
            sub(
                "languages",
                "The languages a service translates.",
                &[optional("service", Of(SERVICES))],
                Now,
            ),
        ],
        None,
    ),
    missing(
        "attach",
        "Attach a file or image to the next message.",
        "files are attached by the screen, with the attachments of turn/start, not by a command",
    ),
    builtin(
        "devices",
        "The cameras and microphones.",
        &[form("The devices.", &[], Now)],
        None,
    ),
    builtin(
        "transcribe",
        "Turn speech in an audio file into text.",
        &[form(
            "The text of the file, by the service given.",
            &[
                arg("file", File),
                arg("service", Of(SPEECH)),
                optional("language", Text),
            ],
            Now,
        )],
        None,
    ),
    builtin(
        "plugins",
        "The installed plugins, searching the catalogs, and installing.",
        &[
            sub("list", "The installed plugins.", &[], Now),
            sub(
                "search",
                "Search the catalogs.",
                &[arg("query", Text)],
                Now,
            ),
            sub(
                "install",
                "Install a plugin, as name@catalog.",
                &[arg("plugin", Text)],
                Now,
            ),
            sub(
                "remove",
                "Delete an installed plugin, as name@catalog.",
                &[arg("plugin", Text)],
                Now,
            ),
            sub(
                "disable",
                "Turn an installed plugin off without deleting it, as name@catalog.",
                &[arg("plugin", Text)],
                Now,
            ),
            sub(
                "enable",
                "Turn an installed plugin back on, as name@catalog.",
                &[arg("plugin", Text)],
                Now,
            ),
        ],
        Some("list"),
    ),
    builtin(
        "skills",
        "The skills of the installed plugins.",
        &[form("The skills.", &[], Now)],
        None,
    ),
    builtin(
        "mcp",
        "The MCP servers and their tools, and signing in to the servers reached over HTTP.",
        &[
            form(
                "The servers, with the sign-in state of each one reached over HTTP.",
                &[],
                Now,
            ),
            sub(
                "login",
                "Start signing in to an MCP server and show the page to open.",
                &[arg("server", Text)],
                Now,
            ),
            sub(
                "status",
                "Whether a sign-in to an MCP server finished.",
                &[arg("login", Text)],
                Now,
            ),
            sub(
                "logout",
                "Remove the sign-in to an MCP server.",
                &[arg("server", Text)],
                Now,
            ),
        ],
        None,
    ),
    builtin(
        "connectors",
        "The Claude and ChatGPT connectors of the signed-in accounts.",
        &[form("The connectors.", &[], Now)],
        None,
    ),
    builtin(
        "init",
        "Have the agent write the project's AGENTS.md.",
        &[turn_form("Write AGENTS.md.", &[])],
        None,
    ),
    builtin(
        "review",
        "Have the agent review the current changes.",
        &[turn_form(
            "Review the changes, with an optional focus.",
            &[optional("focus", Text)],
        )],
        None,
    ),
    builtin(
        "diff",
        "The project's git status and changes.",
        &[form("The diff.", &[], Now)],
        None,
    ),
];

/// Each model tool that keeps a record, and the command that shows it
/// (docs/evidence/slash-commands.md, rule 6).
#[cfg(test)]
const SHOWN_BY: &[(&str, &str)] = &[
    (crate::agent_tools::PLAN, "plan"),
    (crate::promise::PROMISE, "promise"),
    (crate::agent_tools::LESSON, "notes"),
    (crate::agent_tools::METHOD, "notes"),
    (crate::agent_tools::ISSUE, "issues"),
    (crate::goal::GOAL, "goal"),
    (crate::ask::ASK, "answer"),
    (crate::plugins::INSTALL, "plugins"),
    (crate::company::DELEGATE, "members"),
];

/// Model tools that keep no record of their own: they read, act, or write
/// files the model and the person read as files.
#[cfg(test)]
const RECORDS_NOTHING: &[&str] = &[
    crate::agent_tools::RECALL,
    crate::web_search::SEARCH,
    crate::read::READ,
    crate::fetch::FETCH,
    crate::find::FIND,
    crate::explore::EXPLORE,
    crate::camera::CAMERA,
    crate::audio::AUDIO,
    crate::transcribe::TRANSCRIBE,
    crate::plugins::SEARCH,
    crate::edit::EDIT,
    crate::edit::EDITS,
    crate::edit::WRITE,
    crate::shell::SHELL,
    crate::shell::JOB,
    crate::mcp::SEARCH,
    crate::mcp::LIST_RESOURCES,
    crate::mcp::LIST_TEMPLATES,
    crate::mcp::READ_RESOURCE,
    crate::company::REVIEW,
];

const INIT_PROMPT: &str = "Write an AGENTS.md file at the root of this project for coding \
agents that will work on it. Read the project first: its layout, how to build, test and lint \
it, its conventions, and anything a newcomer would get wrong. Keep the file short and \
concrete: commands exactly as they run, paths that exist, and rules the code actually \
follows. If an AGENTS.md exists, improve it instead of replacing what it says.";

const REVIEW_PROMPT: &str = "Review the current uncommitted changes of this project (git \
status and git diff, staged and unstaged). Look for bugs, broken behavior, missing error \
handling, security problems and missing tests. Report each finding with its file and line, \
why it is a problem, and how to fix it, most serious first. Do not change any files.";

/// Where a command that is not built in comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Origin {
    /// A plugin's `commands/<name>.md`.
    PluginCommand(String),
    /// A plugin's skill.
    PluginSkill(String),
    /// `commands/<name>.md` beside the database (`~/.inocode/commands`).
    User,
    /// `.inocode/commands/<name>.md` in the project folder.
    Project,
}

impl Origin {
    fn prefix(&self) -> &str {
        match self {
            Self::PluginCommand(plugin) | Self::PluginSkill(plugin) => plugin,
            Self::User => "user",
            Self::Project => "project",
        }
    }
}

/// A command made of a prompt: a Markdown file, or a plugin's skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Custom {
    pub name: String,
    pub origin: Origin,
    pub description: String,
    pub hint: Option<String>,
    pub path: PathBuf,
}

impl Custom {
    fn full(&self) -> String {
        format!("{}:{}", self.origin.prefix(), self.name)
    }

    /// The prompt it sends for `arguments`.
    fn prompt(&self, arguments: &str) -> Result<String, ErrorInfo> {
        if let Origin::PluginSkill(_) = self.origin {
            let mut prompt = format!(
                "Use the skill {}: read its instructions in {} with the read tool and follow them.",
                self.full(),
                self.path.display()
            );
            if !arguments.is_empty() {
                prompt.push_str(&format!("\n\nARGUMENTS: {arguments}"));
            }
            return Ok(prompt);
        }
        let text = std::fs::read_to_string(&self.path).map_err(|error| {
            ErrorInfo::new(
                "command_unreadable",
                format!("{}: {error}", self.path.display()),
            )
        })?;
        Ok(expand(front_matter(&text).1, arguments))
    }
}

/// A Markdown command's front matter fields and its body.
fn front_matter(text: &str) -> (Vec<(String, String)>, &str) {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let Some(rest) = text
        .strip_prefix("---\r\n")
        .or_else(|| text.strip_prefix("---\n"))
    else {
        return (Vec::new(), text);
    };
    let mut fields = Vec::new();
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        offset += line.len();
        let line = line.trim_end();
        if line == "---" {
            return (fields, &rest[offset..]);
        }
        if let Some((key, value)) = line.split_once(':') {
            let value = value.trim().trim_matches(|c| c == '"' || c == '\'');
            fields.push((key.trim().to_string(), value.to_string()));
        }
    }
    (Vec::new(), text)
}

/// The prompt of a Markdown command's `body` for `arguments`: `$ARGUMENTS`
/// replaced, or, when the body has none, the arguments added at its end as
/// Claude Code does.
fn expand(body: &str, arguments: &str) -> String {
    let body = body.trim();
    if body.contains("$ARGUMENTS") {
        return body.replace("$ARGUMENTS", arguments);
    }
    match arguments.is_empty() {
        true => body.to_string(),
        false => format!("{body}\n\nARGUMENTS: {arguments}"),
    }
}

/// A name that can be typed after `/` and carries no prefix.
fn plain_name(name: &str) -> bool {
    !name.is_empty() && !name.contains(':') && !name.chars().any(char::is_whitespace)
}

/// The Markdown commands in `folder`.
fn markdown_commands(folder: &Path, origin: &Origin) -> Vec<Custom> {
    let Ok(found) = std::fs::read_dir(folder) else {
        return Vec::new();
    };
    let mut files: Vec<(String, PathBuf)> = found
        .flatten()
        .map(|item| item.path())
        .filter(|path| path.is_file() && path.extension().is_some_and(|ext| ext == "md"))
        .filter_map(|path| Some((path.file_stem()?.to_str()?.to_string(), path)))
        .collect();
    files.sort();
    files
        .into_iter()
        .filter_map(|(name, path)| custom(name, path, origin.clone()))
        .collect()
}

fn custom(name: String, path: PathBuf, origin: Origin) -> Option<Custom> {
    if !plain_name(&name) {
        return None;
    }
    let text = std::fs::read_to_string(&path).ok()?;
    let (fields, _) = front_matter(&text);
    let field = |key: &str| {
        fields
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.clone())
            .filter(|value| !value.is_empty())
    };
    Some(Custom {
        name,
        description: field("description").unwrap_or_default(),
        hint: field("argument-hint"),
        path,
        origin,
    })
}

/// The commands of one thread: the built-in ones and the others found.
pub(crate) struct Registry {
    customs: Vec<Custom>,
    /// The prompts of the MCP servers, each `/mcp__<server>__<prompt>`.
    prompts: Vec<crate::mcp::PromptCommand>,
}

/// What a typed name refers to.
pub(crate) enum Target<'a> {
    Builtin(&'static Builtin),
    Custom(&'a Custom),
    Prompt(&'a crate::mcp::PromptCommand),
}

fn builtin_named(name: &str) -> Option<&'static Builtin> {
    BUILTINS
        .iter()
        .find(|command| command.name == name || command.aliases.contains(&name))
}

impl Registry {
    pub(crate) fn new(customs: Vec<Custom>) -> Self {
        Self {
            customs,
            prompts: Vec::new(),
        }
    }

    /// Adds the prompts of the MCP servers; one with a built-in command's
    /// name is left out, since built-in commands are not overridden.
    fn with_prompts(mut self, prompts: Vec<crate::mcp::PromptCommand>) -> Self {
        self.prompts = prompts
            .into_iter()
            .filter(|prompt| builtin_named(&prompt.name).is_none())
            .collect();
        self
    }

    fn prompt_named(&self, name: &str) -> Option<&crate::mcp::PromptCommand> {
        self.prompts.iter().find(|prompt| prompt.name == name)
    }

    /// The commands of the installed plugins under `plugins`, the user's in
    /// `user` and the project's in `project`.
    pub(crate) fn load(plugins: &Path, user: &Path, project: Option<&Path>) -> Self {
        let mut customs = Vec::new();
        for plugin in crate::plugins::Installed::all(plugins) {
            let origin = Origin::PluginCommand(plugin.name().to_string());
            for (name, path) in plugin.commands() {
                customs.extend(custom(name, path, origin.clone()));
            }
            for skill in plugin.skills() {
                let name = skill
                    .name
                    .split_once(':')
                    .map_or(skill.name.as_str(), |(_, name)| name);
                if plain_name(name) {
                    customs.push(Custom {
                        name: name.to_string(),
                        origin: Origin::PluginSkill(plugin.name().to_string()),
                        description: skill.description.clone(),
                        hint: None,
                        path: skill.path.clone(),
                    });
                }
            }
        }
        customs.extend(markdown_commands(user, &Origin::User));
        if let Some(project) = project {
            customs.extend(markdown_commands(project, &Origin::Project));
        }
        Self::new(customs)
    }

    /// Why `custom` cannot be called at all, if it cannot.
    fn blocked(&self, custom: &Custom) -> Option<String> {
        let full = custom.full();
        let same = self.customs.iter().filter(|other| other.full() == full);
        (same.count() > 1).then(|| format!("another command is also named /{full}"))
    }

    /// Whether `custom` may be called by its name alone: no built-in
    /// command has it and no other command shares it.
    fn short_usable(&self, custom: &Custom) -> bool {
        builtin_named(&custom.name).is_none()
            && self.prompt_named(&custom.name).is_none()
            && self
                .customs
                .iter()
                .filter(|other| other.name == custom.name)
                .count()
                == 1
    }

    pub(crate) fn resolve(&self, name: &str) -> Result<Target<'_>, CommandError> {
        if name.contains(':') {
            let found: Vec<&Custom> = self
                .customs
                .iter()
                .filter(|custom| custom.full() == name)
                .collect();
            return match found.as_slice() {
                [custom] => Ok(Target::Custom(custom)),
                [] => Err(unknown(name)),
                _ => Err(CommandError::from(ErrorInfo::new(
                    "ambiguous_command",
                    format!("more than one command is named /{name}"),
                ))),
            };
        }
        if let Some(command) = builtin_named(name) {
            return Ok(Target::Builtin(command));
        }
        if let Some(prompt) = self.prompt_named(name) {
            return Ok(Target::Prompt(prompt));
        }
        let found: Vec<&Custom> = self
            .customs
            .iter()
            .filter(|custom| custom.name == name)
            .collect();
        match found.as_slice() {
            [custom] => Ok(Target::Custom(custom)),
            [] => Err(unknown(name)),
            many => {
                let names: Vec<String> = many
                    .iter()
                    .map(|custom| format!("/{}", custom.full()))
                    .collect();
                Err(CommandError::from(ErrorInfo::new(
                    "ambiguous_command",
                    format!(
                        "/{name} is the name of more than one command; use {}",
                        names.join(" or ")
                    ),
                )))
            }
        }
    }

    pub(crate) fn list(&self) -> Vec<Value> {
        BUILTINS
            .iter()
            .map(builtin_entry)
            .chain(self.customs.iter().map(|custom| self.custom_entry(custom)))
            .chain(self.prompts.iter().map(prompt_entry))
            .collect()
    }

    fn custom_entry(&self, custom: &Custom) -> Value {
        let (source, plugin, kind) = match &custom.origin {
            Origin::PluginCommand(plugin) => ("plugin", Some(plugin.as_str()), "command"),
            Origin::PluginSkill(plugin) => ("plugin", Some(plugin.as_str()), "skill"),
            Origin::User => ("user", None, "command"),
            Origin::Project => ("project", None, "command"),
        };
        let blocked = self.blocked(custom);
        json!({
            "name": custom.full(),
            "short": self.short_usable(custom).then_some(custom.name.as_str()),
            "aliases": [],
            "description": custom.description,
            "source": source,
            "plugin": plugin,
            "kind": kind,
            "path": custom.path,
            "forms": [{
                "sub": null,
                "description": custom.description,
                "usage": format!("/{} [{}]", custom.full(), custom.hint.as_deref().unwrap_or("arguments")),
                "args": [{"name": "arguments", "kind": "text", "required": false, "hint": custom.hint}],
                "when": When::AfterTurn,
                "runs": Runs::Turn,
                "available": blocked.is_none(),
                "reason": blocked,
            }],
            "available": blocked.is_none(),
            "reason": blocked,
        })
    }

    /// Names that reach more than one command, and the names to use.
    fn conflicts(&self) -> Vec<Value> {
        let mut names: Vec<&str> = self.customs.iter().map(|c| c.name.as_str()).collect();
        names.sort();
        names.dedup();
        names
            .into_iter()
            .filter_map(|name| {
                let same: Vec<String> = self
                    .customs
                    .iter()
                    .filter(|custom| custom.name == name)
                    .map(|custom| format!("/{}", custom.full()))
                    .collect();
                let builtin = builtin_named(name).is_some();
                let prompt = self.prompt_named(name).is_some();
                (same.len() > 1 || builtin || prompt).then(|| {
                    json!({
                        "name": name,
                        "builtin": builtin.then(|| format!("/{name}")),
                        "prompt": prompt.then(|| format!("/{name}")),
                        "use": same,
                    })
                })
            })
            .collect()
    }
}

/// An MCP server's prompt as `commands/list` shows it: one argument per
/// declared prompt argument, typed in order, the last taking the rest.
fn prompt_entry(prompt: &crate::mcp::PromptCommand) -> Value {
    let args: Vec<Value> = prompt
        .arguments
        .iter()
        .map(|argument| {
            json!({
                "name": argument.name,
                "kind": "text",
                "required": argument.required,
                "description": argument.description,
            })
        })
        .collect();
    let mut usage = format!("/{}", prompt.name);
    for argument in &prompt.arguments {
        usage.push_str(&match argument.required {
            true => format!(" <{}>", argument.name),
            false => format!(" [{}]", argument.name),
        });
    }
    json!({
        "name": prompt.name,
        "short": prompt.name,
        "aliases": [],
        "description": prompt.description,
        "source": "mcp",
        "plugin": null,
        "kind": "prompt",
        "server": prompt.server,
        "prompt": prompt.prompt,
        "forms": [{
            "sub": null,
            "description": prompt.description,
            "usage": usage,
            "args": args,
            "when": When::AfterTurn,
            "runs": Runs::Turn,
            "available": true,
            "reason": null,
        }],
        "available": true,
        "reason": null,
    })
}

fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Text => "text",
        Kind::Path => "path",
        Kind::Number => "number",
        Kind::Choice(_) => "choice",
    }
}

fn usage(name: &str, form: &Form) -> String {
    let mut text = format!("/{name}");
    if let Some(sub) = form.sub {
        text.push(' ');
        text.push_str(sub);
    }
    for arg in form.args {
        let shown = match arg.kind {
            Kind::Choice(choices) => choices.join("|"),
            _ => arg.name.to_string(),
        };
        text.push_str(&match arg.required {
            true => format!(" <{shown}>"),
            false => format!(" [{shown}]"),
        });
    }
    text
}

fn builtin_entry(command: &'static Builtin) -> Value {
    let forms: Vec<Value> = command
        .forms
        .iter()
        .map(|form| {
            let args: Vec<Value> = form
                .args
                .iter()
                .map(|arg| {
                    let choices = match arg.kind {
                        Kind::Choice(choices) => Some(choices),
                        _ => None,
                    };
                    json!({
                        "name": arg.name,
                        "kind": kind_name(arg.kind),
                        "choices": choices,
                        "required": arg.required,
                    })
                })
                .collect();
            let reason = form.unavailable.or(command.unavailable);
            json!({
                "sub": form.sub,
                "description": form.description,
                "usage": usage(command.name, form),
                "args": args,
                "when": form.when,
                "runs": form.runs,
                "available": reason.is_none(),
                "reason": reason,
            })
        })
        .collect();
    json!({
        "name": command.name,
        "short": command.name,
        "aliases": command.aliases,
        "description": command.description,
        "source": "builtin",
        "plugin": null,
        "kind": "builtin",
        "forms": forms,
        "bare": command.bare,
        "available": command.unavailable.is_none(),
        "reason": command.unavailable,
    })
}

/// The values of `args` in `text`, in order: each takes one word, the last
/// text or path argument the rest of the line.
fn bind(args: &[Arg], text: &str) -> Result<Vec<String>, String> {
    let mut values = Vec::new();
    let mut rest = text.trim();
    for (index, arg) in args.iter().enumerate() {
        if rest.is_empty() {
            if arg.required {
                return Err(match arg.kind {
                    Kind::Choice(choices) => format!("needs {}", choices.join(" or ")),
                    _ => format!("needs <{}>", arg.name),
                });
            }
            break;
        }
        let whole = index + 1 == args.len() && matches!(arg.kind, Kind::Text | Kind::Path);
        let value = if whole {
            std::mem::take(&mut rest)
        } else {
            let (word, after) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            rest = after.trim_start();
            word
        };
        match arg.kind {
            Kind::Number if value.parse::<i64>().is_err() => {
                return Err(format!("<{}> must be a number, not \"{value}\"", arg.name))
            }
            Kind::Choice(choices) if !choices.contains(&value) => {
                return Err(format!(
                    "<{}> must be {}, not \"{value}\"",
                    arg.name,
                    choices.join(" or ")
                ))
            }
            _ => {}
        }
        values.push(value.to_string());
    }
    if !rest.is_empty() {
        return Err(format!("unexpected \"{rest}\""));
    }
    Ok(values)
}

/// The form of `command` that `text` calls, and its values.
fn choose(command: &'static Builtin, text: &str) -> Result<(&'static Form, Vec<String>), String> {
    let text = text.trim();
    let named = |name: &str| command.forms.iter().find(|form| form.sub == Some(name));
    let own = command.forms.iter().find(|form| form.sub.is_none());
    if text.is_empty() {
        if let Some(form) = command.bare.and_then(named) {
            return Ok((form, Vec::new()));
        }
    }
    let (first, after) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
    if let Some(form) = named(first) {
        match bind(form.args, after) {
            Ok(values) => return Ok((form, values)),
            // A subcommand's name may begin free text, such as a goal.
            Err(error) => match own.filter(|own| !own.args.is_empty()) {
                Some(own) => {
                    return bind(own.args, text)
                        .map(|values| (own, values))
                        .map_err(|_| format!("{} {first}: {error}", command.name))
                }
                None => return Err(format!("{} {first}: {error}", command.name)),
            },
        }
    }
    match own {
        Some(own) => bind(own.args, text).map(|values| (own, values)),
        None => {
            let subs: Vec<&str> = command.forms.iter().filter_map(|form| form.sub).collect();
            Err(format!(
                "unknown subcommand \"{first}\"; use one of {}",
                subs.join(", ")
            ))
        }
    }
}

/// Whether a command used `when` may run now; `running` is whether a turn
/// of its thread runs.
pub(crate) fn allowed(when: When, running: bool) -> Result<(), CommandError> {
    match (when, running) {
        (When::AfterTurn, true) => Err(CommandError {
            error: ErrorInfo::new(
                "not_now",
                "this command can be used once the running turn has ended",
            ),
            when: Some(When::AfterTurn),
        }),
        _ => Ok(()),
    }
}

/// A command's failure; `when` says when a command refused now may be
/// used.
#[derive(Debug, Clone, PartialEq)]
pub struct CommandError {
    pub error: ErrorInfo,
    pub when: Option<When>,
}

impl From<ErrorInfo> for CommandError {
    fn from(error: ErrorInfo) -> Self {
        Self { error, when: None }
    }
}

fn unknown(name: &str) -> CommandError {
    CommandError::from(ErrorInfo::new(
        "unknown_command",
        format!("there is no command /{name}; /help lists them"),
    ))
}

fn invalid(message: impl Into<String>) -> ErrorInfo {
    ErrorInfo::new("invalid_arguments", message)
}

fn unavailable(reason: &str) -> CommandError {
    CommandError::from(ErrorInfo::new("unavailable", reason))
}

/// A turn a command starts.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnStart {
    pub prompt: String,
    pub reply_to: Option<i64>,
    /// What the command did before the turn.
    pub result: Value,
}

/// What a command gave.
#[derive(Debug, Clone, PartialEq)]
pub enum Ran {
    Result(Value),
    Turn(TurnStart),
}

/// What `/goal` was given: the options in front of the objective, as
/// `goal/set` takes them.
#[derive(Debug, Clone, PartialEq, Eq)]
struct GoalOptions<'a> {
    check: Option<String>,
    rounds: Option<u32>,
    tokens: Option<u64>,
    objective: &'a str,
}

/// Reads `--check <command>`, `--rounds <n>` and `--tokens <n>` from the
/// start of `text`; the rest is the objective. A command with spaces is
/// written in double quotes.
fn goal_options(text: &str) -> Result<GoalOptions<'_>, String> {
    let mut options = GoalOptions {
        check: None,
        rounds: None,
        tokens: None,
        objective: text.trim(),
    };
    loop {
        let rest = options.objective;
        let Some((flag, after)) = rest.split_once(char::is_whitespace) else {
            break;
        };
        if !matches!(flag, "--check" | "--rounds" | "--tokens") {
            break;
        }
        let after = after.trim_start();
        let (word, remaining) = match after.strip_prefix('"') {
            Some(quoted) => quoted
                .split_once('"')
                .ok_or_else(|| format!("{flag} has an unclosed quote"))?,
            None => after.split_once(char::is_whitespace).unwrap_or((after, "")),
        };
        if word.is_empty() {
            return Err(format!("{flag} needs a value"));
        }
        let number = |name: &str| {
            word.parse::<u64>()
                .map_err(|_| format!("{name} must be a number, not {word:?}"))
        };
        match flag {
            "--check" => options.check = Some(word.to_string()),
            "--rounds" => {
                options.rounds = Some(u32::try_from(number("--rounds")?).unwrap_or(u32::MAX))
            }
            _ => options.tokens = Some(number("--tokens")?),
        }
        options.objective = remaining.trim_start();
    }
    if options.objective.is_empty() {
        return Err("give the objective after the options".into());
    }
    Ok(options)
}

/// The name and arguments of a typed command, `/name arguments`.
pub fn split(text: &str) -> Result<(String, String), ErrorInfo> {
    let text = text.trim();
    let rest = text
        .strip_prefix('/')
        .filter(|rest| !rest.is_empty() && !rest.starts_with(char::is_whitespace))
        .ok_or_else(|| invalid("a command starts with / and its name"))?;
    let (name, arguments) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    Ok((name.to_string(), arguments.trim().to_string()))
}

fn summary_view(summary: &ThreadSummary) -> Value {
    json!({
        "thread_id": summary.id.0,
        "project": summary.project,
        "title": summary.title,
        "created": summary.created,
        "last_activity": summary.last_activity,
        "messages": summary.messages,
        "archived": summary.archived,
    })
}

fn note_view(note: &Note) -> Value {
    json!({
        "id": note.id.0,
        "kind": note.kind.as_str(),
        "title": note.title,
        "status": note.status,
        "tags": note.tags,
        "up": note.up,
        "down": note.down,
        "shared": note.project.is_none(),
    })
}

/// The 1-based `number` of a list of `count`, as an index.
fn index(number: &str, count: usize) -> Result<usize, ErrorInfo> {
    number
        .parse::<usize>()
        .ok()
        .filter(|number| (1..=count).contains(number))
        .map(|number| number - 1)
        .ok_or_else(|| match count {
            0 => invalid("the list is empty"),
            _ => invalid(format!("the number must be from 1 to {count}")),
        })
}

fn id(value: &str) -> NoteId {
    NoteId(value.parse().unwrap_or_default())
}

/// A thread's messages as Markdown.
fn markdown(messages: &[crate::store::StoredMessage]) -> String {
    let mut text = String::new();
    for stored in messages {
        let Message { body, .. } = &stored.message;
        match body {
            Body::User {
                from: Speaker::Person,
                parts,
                ..
            } => text.push_str(&format!(
                "## Person (#{})\n\n{}\n\n",
                stored.seq,
                parts.join("\n")
            )),
            Body::User {
                from: Speaker::Harness(kind),
                parts,
                ..
            } => text.push_str(&format!(
                "## Harness: {kind} (#{})\n\n{}\n\n",
                stored.seq,
                parts.join("\n")
            )),
            Body::Assistant { parts, .. } => {
                text.push_str(&format!("## Assistant (#{})\n\n", stored.seq));
                for part in parts {
                    match part {
                        AssistantPart::Text { text: part } => {
                            text.push_str(part);
                            text.push_str("\n\n");
                        }
                        AssistantPart::ToolCall {
                            name, arguments, ..
                        } => text
                            .push_str(&format!("Call `{name}`:\n\n```json\n{arguments}\n```\n\n")),
                        AssistantPart::Reasoning { .. } => {}
                    }
                }
            }
            Body::ToolResult {
                name,
                content,
                is_error,
                ..
            } => text.push_str(&format!(
                "## {} of `{name}` (#{})\n\n```\n{content}\n```\n\n",
                if *is_error { "Error" } else { "Result" },
                stored.seq
            )),
        }
    }
    text
}

/// A choice of model as typed: `provider/model` when the part before the
/// first `/` names a configured provider, else a model of the thread's
/// provider; `default` returns both to the settings' default.
fn model_patch(settings: &crate::settings::Settings, text: &str) -> Value {
    if text == "default" {
        return json!({"provider": null, "model": null});
    }
    match text.split_once('/') {
        Some((provider, model)) if settings.provider(provider).is_some() => {
            json!({"provider": provider, "model": model})
        }
        _ => json!({"model": text}),
    }
}

/// The output of `git` with `arguments` in `folder`.
async fn git(folder: &Path, arguments: &[&str]) -> Result<String, ErrorInfo> {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(folder)
        .args(arguments)
        .output()
        .await
        .map_err(|error| ErrorInfo::new("git", format!("git could not run: {error}")))?;
    if !output.status.success() {
        return Err(ErrorInfo::new(
            "git",
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

impl Core {
    /// The threads, the most recently active first, for `thread/list`:
    /// only those of the project with the key `project` when given, and
    /// only the archived ones or only the others, as `archived` says.
    pub async fn threads(
        &self,
        project: Option<&str>,
        archived: bool,
        limit: usize,
    ) -> Result<Value, ErrorInfo> {
        let summaries = self
            .store()?
            .sessions(project.map(str::to_string), Some(archived), limit)
            .await?;
        let mut threads = Vec::with_capacity(summaries.len());
        for summary in &summaries {
            let mut view = summary_view(summary);
            view["status"] = json!(self.thread_status(summary.id).await?);
            threads.push(view);
        }
        Ok(json!({ "threads": threads }))
    }

    /// `thread/rename`: gives `thread` the title `title`.
    pub async fn rename_thread(&self, thread: SessionId, title: &str) -> Result<Value, ErrorInfo> {
        if !self.store()?.rename_session(thread, title).await? {
            return Err(crate::unknown_thread());
        }
        self.announce_title(thread, title).await;
        Ok(json!({ "thread_id": thread.0, "title": title }))
    }

    /// `thread/delete` and `/delete confirm`: deletes `thread` for good,
    /// with its commands and files. Refused while a turn runs.
    pub async fn delete_thread(&self, thread: SessionId) -> Result<Value, ErrorInfo> {
        let store = self.store()?;
        if !store.session_exists(thread).await? {
            return Err(crate::unknown_thread());
        }
        if self.turn_running(thread) {
            return Err(ErrorInfo::new(
                "turn_running",
                "a turn is running in this thread; delete it when the turn ends",
            ));
        }
        let folder = store.session_project(thread).await?.map(|(_, key)| key);
        let mut said = self
            .run_hooks(
                thread,
                folder.as_deref().map(std::path::Path::new),
                "SessionEnd",
                Some("thread_deleted"),
                json!({ "reason": "thread_deleted" }),
            )
            .await;
        store.delete_session(thread).await?;
        crate::shell::stop_all(&store.files_folder().join("logs"), thread);
        let kept = store.remove_thread_files(thread).await;
        // The thread's own worktree goes with it, unless work in it would be
        // lost (docs/evidence/thread-folders.md).
        let own = crate::worktree::place(&store.files_folder(), thread.0);
        let worktree = match folder {
            Some(folder) if std::path::Path::new(&folder) == own && own.is_dir() => {
                match crate::worktree::remove_if_clean(&own).await {
                    Ok(()) => {
                        said.extend(
                            self.run_hooks(
                                thread,
                                None,
                                "WorktreeRemove",
                                None,
                                json!({ "worktree_path": folder }),
                            )
                            .await,
                        );
                        json!({ "path": folder, "removed": true })
                    }
                    Err(why) => json!({ "path": folder, "removed": false, "kept_because": why }),
                }
            }
            _ => Value::Null,
        };
        let _ = self
            .activity
            .send((thread, Activity::Changed(ThreadChange::Deleted)));
        Ok(json!({
            "deleted": thread.0, "files_not_removed": kept, "worktree": worktree,
            "hook_messages": said,
        }))
    }

    /// `thread/archive` and `thread/unarchive`. An archived thread is left
    /// out of `thread/list` and refuses new turns with `thread_archived`
    /// until it is unarchived; it can still be read. Archiving is refused
    /// while a turn runs. Those watching are told only when the state
    /// changes.
    pub async fn set_thread_archived(
        &self,
        thread: SessionId,
        archived: bool,
    ) -> Result<Value, ErrorInfo> {
        let store = self.store()?;
        if archived && self.turn_running(thread) {
            return Err(ErrorInfo::new(
                "turn_running",
                "a turn is running in this thread; archive it when the turn ends",
            ));
        }
        let before = store
            .set_archived(thread, archived)
            .await?
            .ok_or_else(crate::unknown_thread)?;
        if before != archived {
            let change = match archived {
                true => ThreadChange::Archived,
                false => ThreadChange::Unarchived,
            };
            let _ = self.activity.send((thread, Activity::Changed(change)));
        }
        Ok(json!({ "thread_id": thread.0, "archived": archived }))
    }

    /// `thread/export`: `thread` as Markdown or JSON text, as `/export`
    /// gives it without a path.
    pub async fn export_thread(&self, thread: SessionId, format: &str) -> Result<Value, ErrorInfo> {
        if !self.store()?.session_exists(thread).await? {
            return Err(crate::unknown_thread());
        }
        self.export(thread, Some(format), None).await
    }

    /// `thread/search` and `/history`: up to `limit` messages of every
    /// thread matching `query`.
    pub async fn search_threads(&self, query: &str, limit: usize) -> Result<Value, ErrorInfo> {
        let found = self
            .store()?
            .search_history(HistoryQuery {
                text: query.to_string(),
                project: None,
                since: None,
                until: None,
                limit,
                budget: Duration::from_secs(2),
            })
            .await?;
        let hits: Vec<Value> = found
            .items
            .iter()
            .map(|hit| {
                json!({
                    "thread_id": hit.session.0, "seq": hit.seq, "role": hit.role,
                    "created": hit.created, "snippet": hit.snippet,
                })
            })
            .collect();
        Ok(json!({ "hits": hits, "truncated": found.truncated }))
    }

    /// The commands `thread` (or a client without one) may call. With
    /// `prompts`, the prompts of the MCP servers are among them: of the
    /// servers already started, or with `Some(true)`, of all, started for
    /// it.
    async fn registry(
        &self,
        thread: Option<SessionId>,
        prompts: Option<bool>,
    ) -> Result<Registry, ErrorInfo> {
        let store = self.store()?;
        let project = match thread {
            Some(thread) => self.project_folder(thread).await?,
            None => None,
        };
        let plugins = crate::plugins::folder(store);
        let user = store.files_folder().with_file_name("commands");
        let project = project.map(|(_, folder)| folder.join(".inocode").join("commands"));
        let registry = tokio::task::spawn_blocking(move || {
            Registry::load(&plugins, &user, project.as_deref())
        })
        .await
        .map_err(|error| ErrorInfo::new("internal", error.to_string()))?;
        Ok(match prompts {
            Some(start) => registry.with_prompts(crate::mcp::prompt_commands(store, start).await),
            None => registry,
        })
    }

    async fn project_folder(
        &self,
        thread: SessionId,
    ) -> Result<Option<(ProjectId, PathBuf)>, ErrorInfo> {
        Ok(self
            .store()?
            .session_project(thread)
            .await?
            .map(|(id, key)| (id, PathBuf::from(key))))
    }

    async fn needs_folder(&self, thread: SessionId) -> Result<PathBuf, ErrorInfo> {
        self.project_folder(thread)
            .await?
            .map(|(_, folder)| folder)
            .ok_or_else(|| {
                ErrorInfo::new(
                    "no_project",
                    "this thread has no project folder; start it with a project",
                )
            })
    }

    /// The provider of the settings this thread's requests use.
    pub(crate) async fn thread_provider(&self, thread: SessionId) -> Result<String, ErrorInfo> {
        self.thread_settings(thread).await?["provider"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| invalid("this thread uses no provider of the settings; name one"))
    }

    /// The commands, for `commands/list`: those of `thread`'s project too
    /// when it is given.
    pub async fn commands(&self, thread: Option<SessionId>) -> Result<Value, ErrorInfo> {
        if let Some(thread) = thread {
            if !self.store()?.session_exists(thread).await? {
                return Err(crate::unknown_thread());
            }
        }
        let registry = self.registry(thread, Some(false)).await?;
        Ok(json!({ "commands": registry.list(), "conflicts": registry.conflicts() }))
    }

    /// Runs the command `name` with `arguments` in `thread`.
    pub async fn run_command(
        &self,
        thread: SessionId,
        name: &str,
        arguments: &str,
    ) -> Result<Ran, CommandError> {
        if !self.store()?.session_exists(thread).await? {
            return Err(crate::unknown_thread().into());
        }
        let name = name.trim().trim_start_matches('/');
        let arguments = arguments.trim();
        // Every prompt's name has the MCP prefix, and one may be run before
        // any turn started the servers; /help lists them too.
        let prompts = match name {
            _ if name.starts_with(crate::mcp::PREFIX) => Some(true),
            "help" => Some(false),
            _ => None,
        };
        let registry = self.registry(Some(thread), prompts).await?;
        let running = self.turn_running(thread);
        match registry.resolve(name)? {
            Target::Prompt(prompt) => {
                allowed(When::AfterTurn, running)?;
                let text = crate::mcp::get_prompt(self.store()?, prompt, arguments).await?;
                // The server's messages are not the person's words.
                let text = crate::review::from_command_file(&prompt.name, arguments, &text);
                Ok(Ran::Turn(TurnStart {
                    prompt: text,
                    reply_to: None,
                    result: json!({ "command": prompt.name, "server": prompt.server }),
                }))
            }
            Target::Custom(custom) => {
                if let Some(reason) = registry.blocked(custom) {
                    return Err(unavailable(&reason));
                }
                allowed(When::AfterTurn, running)?;
                let prompt = custom.prompt(arguments)?;
                // The person's own commands are their words; a project's or a
                // plugin's are not, and must not count as the person's to
                // the reviewer.
                let prompt = match custom.origin {
                    Origin::User => prompt,
                    _ => crate::review::from_command_file(&custom.full(), arguments, &prompt),
                };
                Ok(Ran::Turn(TurnStart {
                    prompt,
                    reply_to: None,
                    result: json!({ "command": custom.full() }),
                }))
            }
            Target::Builtin(command) => {
                if let Some(reason) = command.unavailable {
                    return Err(unavailable(reason));
                }
                let (form, values) = choose(command, arguments)
                    .map_err(|message| CommandError::from(invalid(message)))?;
                if let Some(reason) = form.unavailable {
                    return Err(unavailable(reason));
                }
                allowed(form.when, running)?;
                let ran = crate::pool::in_thread(
                    thread,
                    self.builtin(thread, command.name, form.sub, &values, &registry),
                )
                .await?;
                Ok(match ran {
                    Ran::Result(mut result) => {
                        if let Some(fields) = result.as_object_mut() {
                            fields.insert("command".into(), json!(command.name));
                        }
                        Ran::Result(result)
                    }
                    Ran::Turn(mut start) => {
                        if let Some(fields) = start.result.as_object_mut() {
                            fields.insert("command".into(), json!(command.name));
                        }
                        Ran::Turn(start)
                    }
                })
            }
        }
    }

    async fn builtin(
        &self,
        thread: SessionId,
        name: &str,
        sub: Option<&str>,
        values: &[String],
        registry: &Registry,
    ) -> Result<Ran, CommandError> {
        let store = self.store()?;
        let value = |index: usize| values.get(index).map(String::as_str);
        let project = self.project_folder(thread).await?.map(|(id, _)| id);
        let result = match (name, sub) {
            ("help", _) => match value(0) {
                None => json!({ "commands": registry.list(), "conflicts": registry.conflicts() }),
                Some(asked) => {
                    let asked = asked.trim_start_matches('/');
                    match registry.resolve(asked)? {
                        Target::Builtin(command) => json!({ "help": builtin_entry(command) }),
                        Target::Custom(custom) => json!({ "help": registry.custom_entry(custom) }),
                        Target::Prompt(prompt) => json!({ "help": prompt_entry(prompt) }),
                    }
                }
            },
            ("new", _) => {
                let key = store.session_project(thread).await?.map(|(_, key)| key);
                let new = self.start_thread(key.as_deref()).await?;
                json!({ "thread_id": new.0, "project": key })
            }
            ("resume", _) => match value(0) {
                None => {
                    let threads: Vec<Value> = store
                        .sessions(None, None, 50)
                        .await?
                        .iter()
                        .map(summary_view)
                        .collect();
                    json!({ "threads": threads })
                }
                Some(chosen) => {
                    let chosen = SessionId(chosen.parse().unwrap_or_default());
                    let summary = store
                        .session_summary(chosen)
                        .await?
                        .ok_or_else(crate::unknown_thread)?;
                    json!({ "thread": summary_view(&summary) })
                }
            },
            ("cd", _) => {
                self.move_thread(thread, value(0).unwrap_or_default())
                    .await?
            }
            ("worktree", _) => self.add_worktree(thread, value(0), None).await?,
            ("rename", _) => {
                let title = value(0).unwrap_or_default();
                store.rename_session(thread, title).await?;
                self.announce_title(thread, title).await;
                json!({ "thread_id": thread.0, "title": title })
            }
            ("fork", _) => {
                let fork = store.fork_session(thread).await?;
                self.pool.note_lineage(fork, thread).await;
                let _ = self.activity.send((
                    fork,
                    Activity::Changed(ThreadChange::Created { from: Some(thread) }),
                ));
                json!({ "thread_id": fork.0, "from": thread.0 })
            }
            ("delete", _) => self.delete_thread(thread).await?,
            ("compact", _) => self.compact(thread, value(0)).await?,
            ("stop", _) => json!({ "interrupted": self.interrupt(thread) }),
            ("export", _) => self.export(thread, value(0), value(1)).await?,
            ("history", _) => {
                self.search_threads(value(0).unwrap_or_default(), 20)
                    .await?
            }
            ("goal", None) => {
                let options = goal_options(value(0).unwrap_or_default()).map_err(invalid)?;
                let objective = options.objective;
                let goal = self
                    .set_goal(
                        thread,
                        objective,
                        options.check.as_deref(),
                        options.rounds,
                        options.tokens,
                    )
                    .await?;
                return Ok(Ran::Turn(TurnStart {
                    prompt: objective.to_string(),
                    reply_to: None,
                    result: json!({ "goal": goal }),
                }));
            }
            ("goal", Some("status")) => self.goal_status(thread).await?,
            ("goal", Some(_)) => json!({ "goal": self.stop_goal(thread).await? }),
            ("plan", Some(sub)) => {
                let settings = self
                    .set_thread_settings(thread, &json!({ "plan": sub == "on" }))
                    .await?;
                json!({ "plan": settings["plan"], "applies": When::NextRequest })
            }
            ("plan", None) => {
                let items = match store.plan(thread).await? {
                    Some(text) => serde_json::from_str(&text)
                        .map_err(|error| ErrorInfo::new("store", error.to_string()))?,
                    None => json!([]),
                };
                json!({ "items": items })
            }
            ("promise", sub) => self.promise(thread, sub, value(0)).await?,
            ("issues", Some("list")) => {
                let issues: Vec<Value> = store
                    .issues(project, false, 100)
                    .await?
                    .iter()
                    .map(|issue| {
                        json!({
                            "id": issue.id.0, "title": issue.title, "state": issue.state,
                            "priority": issue.priority,
                            "blocked_by": issue.blocked_by.iter().map(|id| id.0).collect::<Vec<_>>(),
                        })
                    })
                    .collect();
                json!({ "issues": issues })
            }
            ("issues", Some(action)) => {
                use crate::rules::{Actor, IssueState};
                let number = id(value(0).unwrap_or_default());
                let (issue, _) = store
                    .issue(project, number)
                    .await?
                    .ok_or_else(|| invalid(format!("this project has no issue {}", number.0)))?;
                let (state, done) = match action {
                    "done" => (IssueState::Done, "done"),
                    _ => (IssueState::WontFix, "wontfix"),
                };
                let reason = value(1).unwrap_or("closed by the person");
                store
                    .set_issue_state(issue.id, state, Actor::Person, None, reason)
                    .await?;
                json!({ "id": issue.id.0, "state": done })
            }
            ("notes", sub) => self.notes(project, sub, values).await?,
            ("rules", sub) => self.rules(thread, project, sub, value(0)).await?,
            ("colleague", _) => self.colleague(thread).await?,
            ("members", sub) => self.members_command(thread, sub, values).await?,
            ("allow", sub) => self.allow_command(thread, sub, values).await?,
            ("mode", _) => match value(0) {
                None => json!({ "mode": self.thread_settings(thread).await?["mode"] }),
                Some(mode) => {
                    let settings = self
                        .set_thread_settings(thread, &json!({ "mode": mode }))
                        .await?;
                    json!({ "mode": settings["mode"], "applies": When::NextRequest })
                }
            },
            ("answer", _) => return self.answer_question(thread, value(0), value(1)).await,
            ("sandbox", _) => {
                let mode = self.thread_mode(thread).await?.unwrap_or_default();
                let enabled = crate::sandbox::enabled();
                let folder = self.project_folder(thread).await?.map(|(_, folder)| folder);
                let mut writable: Vec<String> = folder
                    .iter()
                    .map(|folder| folder.display().to_string())
                    .collect();
                writable.push(
                    store
                        .files_folder()
                        .join("logs")
                        .join("tmp")
                        .display()
                        .to_string(),
                );
                json!({
                    "enabled": enabled,
                    "mode": mode.as_str(),
                    "confined": enabled && mode != turn::Mode::Full,
                    "writable": writable,
                })
            }
            ("model", _) => match value(0) {
                None => self.thread_settings(thread).await?,
                Some(model) => {
                    let patch = model_patch(&self.settings().await?, model);
                    let mut settings = self.set_thread_settings(thread, &patch).await?;
                    settings["applies"] = json!(When::NextRequest);
                    settings
                }
            },
            ("effort", _) => match value(0) {
                None => json!({ "effort": self.thread_settings(thread).await?["effort"] }),
                Some(effort) => {
                    let effort = (effort != "default").then_some(effort);
                    let settings = self
                        .set_thread_settings(thread, &json!({ "effort": effort }))
                        .await?;
                    json!({ "effort": settings["effort"], "applies": When::NextRequest })
                }
            },
            ("roles", _) => match (value(0), value(1)) {
                (None, _) => json!({ "roles": self.get_settings().await?["roles"] }),
                (Some(_), None) => return Err(invalid("give the role's model, or default").into()),
                (Some(role), Some(model)) => {
                    let chosen = match model {
                        "default" => Value::Null,
                        model => match model_patch(&self.settings().await?, model) {
                            patch if patch.get("provider").is_some() => patch,
                            patch => json!({ "model": patch["model"] }),
                        },
                    };
                    let settings = self
                        .set_settings(&json!({ "roles": { role: chosen } }))
                        .await?;
                    json!({ "roles": settings["roles"], "applies": "next_turn" })
                }
            },
            ("providers", Some("list")) => self.providers().await?,
            ("providers", Some("add")) => {
                self.add_provider(&json!({ "id": value(0), "kind": value(1) }))
                    .await?
            }
            ("providers", Some("set")) => {
                let field = value(1).unwrap_or_default();
                let text = value(2).unwrap_or_default();
                let parsed = serde_json::from_str::<Value>(text).unwrap_or_else(|_| json!(text));
                let mut patch = json!({ "id": value(0) });
                patch[field] = parsed;
                self.update_provider(&patch).await?
            }
            ("providers", _) => self.remove_provider(&json!({ "id": value(0) })).await?,
            ("login", None) => {
                let provider = match value(0) {
                    Some(provider) => provider.to_string(),
                    None => self.thread_provider(thread).await?,
                };
                self.login_start(&json!({ "provider": provider, "mode": value(1) }))
                    .await?
            }
            ("login", Some("code")) => {
                self.login_code(&json!({ "login_id": value(0), "code": value(1) }))
                    .await?
            }
            ("login", Some("status")) => {
                self.login_status(&json!({ "login_id": value(0) })).await?
            }
            ("login", Some(_)) => self.login_cancel(&json!({ "login_id": value(0) })).await?,
            ("logout", _) => {
                let provider = self.thread_provider(thread).await?;
                let account = match value(0) {
                    Some(account) => account.to_string(),
                    None => self.accounts_list(&json!({ "provider": provider })).await?["accounts"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .find(|account| account["active"] == true)
                        .and_then(|account| account["id"].as_str())
                        .map(str::to_string)
                        .ok_or_else(|| {
                            invalid(format!(
                                "{provider} uses no single account; name the account to sign out of"
                            ))
                        })?,
                };
                self.remove_account(&json!({ "provider": provider, "account": account }))
                    .await?
            }
            ("accounts", Some("list")) => {
                self.accounts_list(&json!({ "provider": value(0) })).await?
            }
            ("accounts", Some("use")) => {
                self.use_account(&json!({ "provider": value(0), "account": value(1) }))
                    .await?
            }
            ("accounts", Some(pause @ ("pause" | "resume"))) => {
                self.pause_account(&json!({
                    "provider": value(0), "account": value(1), "paused": pause == "pause",
                }))
                .await?
            }
            ("accounts", Some("rename")) => {
                self.rename_account(&json!({
                    "provider": value(0), "account": value(1), "alias": value(2),
                }))
                .await?
            }
            ("accounts", Some("priority")) => {
                let priority = value(2)
                    .and_then(|priority| priority.parse::<i64>().ok())
                    .ok_or_else(|| invalid("give the priority as a number"))?;
                self.set_account_priority(&json!({
                    "provider": value(0), "account": value(1), "priority": priority,
                }))
                .await?
            }
            ("accounts", Some("clear-cooldown")) => {
                self.clear_account_cooldown(&json!({
                    "provider": value(0), "account": value(1),
                }))
                .await?
            }
            ("accounts", Some("strategy")) => {
                self.pool_set(&json!({ "provider": value(0), "strategy": value(1) }))
                    .await?
            }
            ("accounts", Some("status")) => {
                self.pool_status(&json!({ "provider": value(0) })).await?
            }
            ("accounts", _) => {
                self.remove_account(&json!({ "provider": value(0), "account": value(1) }))
                    .await?
            }
            ("models", _) => self.model_catalog(value(0), false).await?,
            ("usage", _) => serde_json::to_value(self.usage().await?)
                .map_err(|error| ErrorInfo::new("internal", error.to_string()))?,
            ("stats", _) => {
                let days = value(0)
                    .and_then(|days| days.parse::<u64>().ok())
                    .unwrap_or(30);
                if !(1..=3_650).contains(&days) {
                    return Err(invalid("the days must be from 1 to 3650").into());
                }
                // Hours in the server's time zone, where the person typing
                // the command is.
                let offset = i64::from(chrono::Local::now().offset().local_minus_utc()) / 60;
                json!({ "models": self.model_stats(days, offset).await? })
            }
            ("status", _) => {
                let summary = store.session_summary(thread).await?;
                let usage = store.session_usage(thread).await?;
                let goal = self.goal(thread).await?;
                json!({
                    "thread": summary.as_ref().map(summary_view),
                    "settings": self.thread_settings(thread).await?,
                    "running": self.turn_running(thread),
                    "usage": {
                        "requests": usage.requests, "input": usage.input,
                        "output": usage.output, "cached": usage.cached,
                    },
                    "goal": goal.map(|goal| json!({"objective": goal.objective, "state": goal.state})),
                })
            }
            ("estimate", _) => {
                self.estimate(Some(thread), value(0).unwrap_or_default())
                    .await?
            }
            ("translate", sub) => self.translate(sub, values).await?,
            ("devices", _) => {
                let folder = store.files_folder().join("images");
                let cameras = crate::camera::run(r#"{"action":"list"}"#, &folder)
                    .await
                    .map(|(text, _)| text);
                let audio_folder = store.files_folder().join("audio");
                let microphones =
                    crate::audio::run(r#"{"action":"list"}"#, None, &audio_folder).await;
                let shown = |found: Result<String, String>| match found {
                    Ok(text) => json!({ "list": text }),
                    Err(error) => json!({ "error": error }),
                };
                json!({ "cameras": shown(cameras), "microphones": shown(microphones) })
            }
            ("transcribe", _) => {
                let folder = self.project_folder(thread).await?.map(|(_, folder)| folder);
                let arguments = json!({
                    "path": value(0), "service": value(1), "language": value(2),
                })
                .to_string();
                let text = crate::transcribe::run(
                    &arguments,
                    folder.as_deref(),
                    &crate::connectors::store_beside(store),
                    &self.settings_path()?,
                    &crate::transcribe::Hosts::default(),
                )
                .await
                .map_err(|error| ErrorInfo::new("transcribe", error))?;
                json!({ "text": text })
            }
            ("plugins", Some("list")) => self.plugins().await?,
            ("plugins", Some("search")) => {
                self.search_plugins(value(0).unwrap_or_default()).await?
            }
            ("plugins", Some("remove")) => self.remove_plugin(value(0).unwrap_or_default()).await?,
            ("plugins", Some(sub @ ("enable" | "disable"))) => {
                self.enable_plugin(value(0).unwrap_or_default(), sub == "enable")
                    .await?
            }
            ("plugins", _) => self.install_plugin(value(0).unwrap_or_default()).await?,
            ("skills", _) => {
                let skills: Vec<Value> = registry
                    .customs
                    .iter()
                    .filter(|custom| matches!(custom.origin, Origin::PluginSkill(_)))
                    .map(|skill| {
                        json!({
                            "name": skill.full(), "description": skill.description,
                            "path": skill.path, "command": format!("/{}", skill.full()),
                        })
                    })
                    .collect();
                json!({ "skills": skills })
            }
            ("mcp", None) => crate::mcp::listed(store).await,
            ("mcp", Some(sub)) => {
                let (method, params) = match sub {
                    "login" => ("mcp/login/start", json!({ "server": value(0) })),
                    "status" => ("mcp/login/status", json!({ "login_id": value(0) })),
                    _ => ("mcp/logout", json!({ "server": value(0) })),
                };
                match crate::mcp::sign_in_request(store, method, &params).await {
                    Some(answer) => answer?,
                    None => unreachable!("{method} is a sign-in request"),
                }
            }
            ("connectors", _) => {
                let servers = crate::connectors::servers(
                    &crate::connectors::store_beside(store),
                    &crate::connectors::Hosts::default(),
                )
                .await;
                let listed: Vec<Value> = servers
                    .iter()
                    .map(|server| {
                        let account = match server.account {
                            crate::connectors::Account::Claude => "claude",
                            crate::connectors::Account::ChatGpt => "chatgpt",
                        };
                        json!({ "name": server.name, "url": server.url, "account": account })
                    })
                    .collect();
                json!({ "connectors": listed })
            }
            ("init", _) => {
                self.needs_folder(thread).await?;
                return Ok(Ran::Turn(TurnStart {
                    prompt: INIT_PROMPT.to_string(),
                    reply_to: None,
                    result: json!({}),
                }));
            }
            ("review", _) => {
                self.needs_folder(thread).await?;
                let mut prompt = REVIEW_PROMPT.to_string();
                if let Some(focus) = value(0) {
                    prompt.push_str(&format!("\n\nFocus on: {focus}"));
                }
                return Ok(Ran::Turn(TurnStart {
                    prompt,
                    reply_to: None,
                    result: json!({}),
                }));
            }
            ("diff", _) => {
                let folder = self.needs_folder(thread).await?;
                let status = git(&folder, &["status", "--short"]).await?;
                let diff = git(&folder, &["diff", "HEAD"]).await.or_else(|_| {
                    // A repository without a commit yet has no HEAD.
                    Ok::<_, ErrorInfo>(String::new())
                })?;
                json!({ "status": status, "diff": diff })
            }
            (other, _) => return Err(unknown(other)),
        };
        Ok(Ran::Result(result))
    }

    async fn compact(&self, thread: SessionId, focus: Option<&str>) -> Result<Value, ErrorInfo> {
        let settings = self.settings().await?;
        let choice = Choice::of(&self.thread_choice(thread).await?, &settings.default);
        // Claimed like a turn, so no turn starts while the summary is made.
        // Nothing reads steering or interrupts here, so they are refused
        // rather than lost.
        let crate::Claim {
            steer,
            model,
            cancel,
            ..
        } = self.begin_turn(thread).await?;
        drop((steer, cancel));
        let main = model.borrow().clone();
        // Read without failing: the turn is claimed, and must be ended.
        let folder = match self.store() {
            Ok(store) => store.session_project(thread).await.ok().flatten(),
            Err(_) => None,
        }
        .map(|(_, key)| key);
        let folder = folder.as_deref().map(std::path::Path::new);
        let mut said = self
            .run_hooks(
                thread,
                folder,
                "PreCompact",
                Some("manual"),
                json!({ "trigger": "manual", "custom_instructions": focus.unwrap_or_default() }),
            )
            .await;
        let _ = self.activity.send((
            thread,
            crate::Activity::Event(turn::TurnEvent::CompactionStarted),
        ));
        let compacted = async {
            let compactor =
                self.role_client(&settings, &choice, settings.roles.compact.as_ref(), &main)?;
            turn::compact_now(&main, compactor.as_deref(), self.store()?, thread, focus).await
        }
        .await;
        let ended = compacted.as_ref().ok().and_then(|(seq, _)| *seq);
        let _ = self.activity.send((
            thread,
            crate::Activity::Event(turn::TurnEvent::CompactionEnded { seq: ended }),
        ));
        said.extend(
            self.run_hooks(
                thread,
                folder,
                "PostCompact",
                Some("manual"),
                json!({ "trigger": "manual" }),
            )
            .await,
        );
        self.end_turn(thread);
        let (seq, warning) = compacted?;
        let mut result = json!({ "compacted": seq.is_some(), "seq": seq, "hook_messages": said });
        if let Some(warning) = warning {
            result["warnings"] = json!([warning]);
        }
        Ok(result)
    }

    async fn export(
        &self,
        thread: SessionId,
        format: Option<&str>,
        path: Option<&str>,
    ) -> Result<Value, ErrorInfo> {
        let store = self.store()?;
        let messages = store.since(thread, 0, i64::MAX as usize).await?;
        let format = format.unwrap_or("markdown");
        let text = match format {
            "json" => {
                let listed: Vec<Value> = messages
                    .iter()
                    .map(|stored| {
                        json!({ "seq": stored.seq, "created": stored.created, "message": stored.message })
                    })
                    .collect();
                serde_json::to_string_pretty(&listed)
                    .map_err(|error| ErrorInfo::new("internal", error.to_string()))?
            }
            _ => markdown(&messages),
        };
        let Some(path) = path else {
            return Ok(json!({ "format": format, "text": text }));
        };
        let path = PathBuf::from(path);
        let path = match self.project_folder(thread).await? {
            Some((_, folder)) if path.is_relative() => folder.join(path),
            _ => path,
        };
        std::fs::write(&path, text)
            .map_err(|error| ErrorInfo::new("export", format!("{}: {error}", path.display())))?;
        Ok(json!({ "format": format, "path": path }))
    }

    async fn goal_status(&self, thread: SessionId) -> Result<Value, ErrorInfo> {
        let Some(goal) = self.goal(thread).await? else {
            return Ok(json!({ "goal": null }));
        };
        let span = self.store()?.span(thread, goal.from_seq).await?;
        let running = self.turn_running(thread);
        let elapsed = span.map_or(0, |(first, last)| {
            let end = match running && goal.is_active() {
                true => crate::store::now(),
                false => last,
            };
            (end - first).max(0)
        });
        let estimate = self.goal_estimate(thread, &goal, elapsed as u64).await?;
        Ok(json!({
            "goal": goal,
            "running": running,
            "elapsed_ms": elapsed,
            "tokens": goal.tokens,
            "token_budget": goal.token_budget,
            "rounds": goal.rounds,
            "round_limit": goal.round_limit,
            "last_check": goal.reason,
            "estimate": estimate,
        }))
    }

    async fn promise(
        &self,
        thread: SessionId,
        sub: Option<&str>,
        number: Option<&str>,
    ) -> Result<Value, ErrorInfo> {
        let store = self.store()?;
        let (text, seq) = store.promises(thread).await?.unwrap_or(("[]".into(), 0));
        let mut items: Vec<Value> = serde_json::from_str(&text)
            .map_err(|error| ErrorInfo::new("store", error.to_string()))?;
        match sub {
            Some("remove") => {
                let removed = items.remove(index(number.unwrap_or_default(), items.len())?);
                store
                    .set_promises(thread, json!(items).to_string(), seq)
                    .await?;
                Ok(json!({ "removed": removed, "items": items }))
            }
            Some(_) => {
                store.set_promises(thread, "[]".into(), seq).await?;
                Ok(json!({ "removed": items, "items": [] }))
            }
            None => Ok(json!({ "items": items })),
        }
    }

    /// A lesson or method of `project` or the shared scope.
    async fn method_note(
        &self,
        project: Option<ProjectId>,
        number: &str,
    ) -> Result<Note, ErrorInfo> {
        let id = id(number);
        self.store()?
            .note(id)
            .await?
            .filter(|note| matches!(note.kind, NoteKind::Lesson | NoteKind::Procedure))
            .filter(|note| note.project.is_none() || note.project == project)
            .ok_or_else(|| invalid(format!("there is no lesson or method {}", id.0)))
    }

    async fn notes(
        &self,
        project: Option<ProjectId>,
        sub: Option<&str>,
        values: &[String],
    ) -> Result<Value, ErrorInfo> {
        let store = self.store()?;
        let value = |index: usize| values.get(index).map(String::as_str).unwrap_or_default();
        let kinds = vec![NoteKind::Lesson, NoteKind::Procedure];
        match sub {
            Some("search") => {
                let found = store
                    .search_notes(NoteQuery {
                        text: value(0).to_string(),
                        project,
                        include_shared: true,
                        kinds,
                        usable_only: false,
                        since: None,
                        until: None,
                        limit: 20,
                        budget: Duration::from_secs(2),
                    })
                    .await?;
                let hits: Vec<Value> = found
                    .items
                    .iter()
                    .map(|hit| {
                        json!({
                            "id": hit.id.0, "kind": hit.kind.as_str(), "title": hit.title,
                            "summary": hit.summary, "snippet": hit.snippet,
                        })
                    })
                    .collect();
                Ok(json!({ "notes": hits, "truncated": found.truncated }))
            }
            Some("show") => {
                let note = self.method_note(project, value(0)).await?;
                let links: Vec<Value> = store
                    .links_of(note.id)
                    .await?
                    .iter()
                    .map(|(other, kind, outgoing)| {
                        json!({ "note": other.0, "kind": kind, "outgoing": outgoing })
                    })
                    .collect();
                let mut shown = note_view(&note);
                shown["summary"] = json!(note.summary);
                shown["condition"] = json!(note.condition);
                shown["body"] = json!(note.body);
                shown["source"] = json!(note.source);
                shown["created"] = json!(note.created);
                shown["last_used"] = json!(note.last_used);
                shown["links"] = json!(links);
                Ok(json!({ "note": shown }))
            }
            Some("edit") => {
                let old = self.method_note(project, value(0)).await?;
                let new = store
                    .add_note(NewNote {
                        project: old.project,
                        kind: old.kind,
                        title: old.title.clone(),
                        summary: old.summary.clone(),
                        keywords: old.keywords.clone(),
                        condition: old.condition.clone(),
                        body: value(1).to_string(),
                        source: NoteSource::Person,
                        tags: old.tags.clone(),
                    })
                    .await?;
                store.supersede(old.id, new).await?;
                Ok(json!({ "id": new.0, "replaces": old.id.0 }))
            }
            Some("tag") => {
                let note = self.method_note(project, value(0)).await?;
                let tags: Vec<String> = value(1).split_whitespace().map(str::to_string).collect();
                store.tag(note.id, tags).await?;
                let tagged = store.note(note.id).await?.map(|note| note.tags);
                Ok(json!({ "id": note.id.0, "tags": tagged }))
            }
            Some("vote") => {
                let note = self.method_note(project, value(0)).await?;
                store.vote(note.id, value(1) == "up").await?;
                Ok(json!({ "id": note.id.0, "vote": value(1) }))
            }
            Some("retire") => {
                let note = self.method_note(project, value(0)).await?;
                store.set_status(note.id, "retired").await?;
                Ok(json!({ "id": note.id.0, "status": "retired" }))
            }
            _ => {
                let notes: Vec<Value> = store
                    .notes_of(project, kinds, 100)
                    .await?
                    .iter()
                    .map(note_view)
                    .collect();
                Ok(json!({ "notes": notes }))
            }
        }
    }

    async fn rules(
        &self,
        thread: SessionId,
        project: Option<ProjectId>,
        sub: Option<&str>,
        number: Option<&str>,
    ) -> Result<Value, ErrorInfo> {
        let store = self.store()?;
        match sub {
            Some("import") => {
                let folder = self.needs_folder(thread).await?;
                let path = folder.join("AGENTS.md");
                let document = std::fs::read_to_string(&path).map_err(|error| {
                    ErrorInfo::new("rules", format!("{}: {error}", path.display()))
                })?;
                let llm = self.thread_client(thread).await?;
                let report = store
                    .import_rules(Some(llm), project, "AGENTS.md", &document)
                    .await?;
                Ok(json!({
                    "added": report.added,
                    "retired": report.retired,
                    "unchanged": report.unchanged,
                    "extraction_error": report.extraction_error,
                    "unverified_sources": report.unverified_sources,
                    "uncovered": report.uncovered,
                }))
            }
            Some("off") => {
                let id = id(number.unwrap_or_default());
                store
                    .note(id)
                    .await?
                    .filter(|note| note.kind == NoteKind::Rule)
                    .filter(|note| note.project.is_none() || note.project == project)
                    .ok_or_else(|| invalid(format!("there is no rule {}", id.0)))?;
                store.set_status(id, "retired").await?;
                Ok(json!({ "id": id.0, "status": "retired" }))
            }
            _ => {
                let mut rules = Vec::new();
                for note in store.notes_of(project, vec![NoteKind::Rule], 200).await? {
                    let strength = store.rule_strength(note.id).await.ok();
                    let mut shown = note_view(&note);
                    shown["strength"] = json!(strength.map(|strength| strength.as_str()));
                    shown["body"] = json!(note.body);
                    rules.push(shown);
                }
                Ok(json!({ "rules": rules }))
            }
        }
    }

    async fn colleague(&self, thread: SessionId) -> Result<Value, ErrorInfo> {
        let settings = self.settings().await?;
        let role = match &settings.roles.colleague {
            Some(role) => json!(role),
            None => Value::Null,
        };
        let advice: Vec<Value> = self
            .store()?
            .since(thread, 0, i64::MAX as usize)
            .await?
            .iter()
            .filter_map(|stored| match &stored.message.body {
                Body::User {
                    from: Speaker::Harness(kind),
                    parts,
                    ..
                } if kind == crate::colleague::KIND => Some(json!({
                    "seq": stored.seq, "created": stored.created, "text": parts.join("\n"),
                })),
                _ => None,
            })
            .collect();
        Ok(json!({ "on": !role.is_null(), "colleague": role, "advice": advice }))
    }

    /// `/answer`: the questions of `ask` from the thread's latest turn and
    /// those of MCP servers its running turn waits on, or the answer to the
    /// one numbered.
    async fn answer_question(
        &self,
        thread: SessionId,
        number: Option<&str>,
        answer: Option<&str>,
    ) -> Result<Ran, CommandError> {
        let asked = self.waiting(thread).await?.questions;
        let store = self.store()?;
        let mut servers: Vec<(u64, Value, bool)> = store
            .elicitations(thread)
            .into_iter()
            .map(|(id, question)| (id, question, false))
            .collect();
        servers.extend(
            store
                .expired_elicitations(thread)
                .await?
                .into_iter()
                .map(|(id, question)| (id, question, true)),
        );
        let count = asked.len() + servers.len();
        let Some(number) = number else {
            let mut listed: Vec<Value> = asked
                .iter()
                .map(|asked| json!({ "kind": "ask", "seq": asked.seq, "questions": asked.questions }))
                .collect();
            listed.extend(servers.iter().map(|(id, question, expired)| {
                json!({
                    "kind": "mcp", "elicitation": id, "question": question, "expired": expired,
                })
            }));
            for (index, item) in listed.iter_mut().enumerate() {
                item["number"] = json!(index + 1);
            }
            return Ok(Ran::Result(
                json!({ "command": "answer", "questions": listed }),
            ));
        };
        let chosen = index(number, count)?;
        let answer = answer
            .filter(|answer| !answer.trim().is_empty())
            .ok_or_else(|| invalid("give the answer after the number"))?;
        if let Some(asked) = asked.get(chosen) {
            allowed(When::AfterTurn, self.turn_running(thread))?;
            let seq = asked.seq.ok_or_else(|| {
                invalid("this question is not in the thread's history; answer it in a message")
            })?;
            return Ok(Ran::Turn(TurnStart {
                prompt: answer.to_string(),
                reply_to: Some(seq),
                result: json!({ "command": "answer", "reply_to": seq }),
            }));
        }
        let (elicitation, _, expired) = &servers[chosen - asked.len()];
        if *expired {
            // The turn that waited for it ended with the server's earlier
            // run, so no answer can reach the MCP server; it is removed.
            store.drop_expired_elicitation(thread, *elicitation).await?;
            return Err(ErrorInfo::new(
                "expired",
                "this question of an MCP server expired when the server restarted; it was \
                 removed, and the server asks again when the work needs it",
            )
            .into());
        }
        let reply = match answer.trim() {
            "decline" | "cancel" => json!({ "action": answer.trim() }),
            content => {
                let content: Value = serde_json::from_str(content)
                    .ok()
                    .filter(Value::is_object)
                    .ok_or_else(|| {
                        invalid("answer an MCP question with a JSON object, decline or cancel")
                    })?;
                json!({ "action": "accept", "content": content })
            }
        };
        let accepted = self.answer(thread, *elicitation, reply);
        Ok(Ran::Result(
            json!({ "command": "answer", "accepted": accepted }),
        ))
    }

    async fn translate(&self, sub: Option<&str>, values: &[String]) -> Result<Value, ErrorInfo> {
        let value = |index: usize| values.get(index).map(String::as_str);
        match sub {
            Some("thinking") => {
                self.set_translation(&json!({ "thinking": value(0) == Some("on") }))
                    .await
            }
            Some("answer") => Err(ErrorInfo::new(
                "not_implemented",
                "translating answers is not implemented",
            )),
            Some("service") => self.set_translation(&json!({ "service": value(0) })).await,
            Some("to") => self.set_translation(&json!({ "to": value(0) })).await,
            Some("models") => match value(0) {
                Some(_) => {
                    let (service, from, to) = match (value(1), value(2), value(3)) {
                        (Some(service), Some(from), Some(to)) => (service, from, to),
                        _ => return Err(invalid("models remove needs <service> <from> <to>")),
                    };
                    self.remove_translation_model(
                        &json!({ "service": service, "from": from, "to": to }),
                    )
                    .await
                }
                None => self.translation_models().await,
            },
            Some(_) => {
                let service = match value(0) {
                    Some(service) => service.to_string(),
                    None => self.translation_view().await?["service"]
                        .as_str()
                        .unwrap_or("google")
                        .to_string(),
                };
                self.translation_languages(&json!({ "service": service }))
                    .await
            }
            None => self.translation_view().await,
        }
    }

    async fn translation_view(&self) -> Result<Value, ErrorInfo> {
        let path = self.settings_path()?;
        let settings = crate::translate::Settings::read(&path)
            .await
            .map_err(|error| ErrorInfo::new("settings_invalid", error))?;
        Ok(json!({
            "thinking": settings.thinking,
            "service": settings.service.name(),
            "to": settings.to,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn custom(name: &str, origin: Origin) -> Custom {
        Custom {
            name: name.into(),
            origin,
            description: String::new(),
            hint: None,
            path: PathBuf::from(format!("{name}.md")),
        }
    }

    fn resolved(registry: &Registry, name: &str) -> String {
        match registry.resolve(name) {
            Ok(Target::Builtin(command)) => format!("builtin:{}", command.name),
            Ok(Target::Custom(custom)) => custom.full(),
            Ok(Target::Prompt(prompt)) => prompt.name.clone(),
            Err(error) => error.error.code.to_string(),
        }
    }

    #[test]
    fn a_typed_command_is_split_into_its_name_and_arguments() {
        assert_eq!(
            split("/goal fix the tests").unwrap(),
            ("goal".to_string(), "fix the tests".to_string())
        );
        assert_eq!(
            split("  /status ").unwrap(),
            ("status".into(), String::new())
        );
        assert!(split("goal").is_err());
        assert!(split("/").is_err());
        assert!(split("/ goal").is_err());
    }

    #[test]
    fn built_in_names_cannot_be_taken_by_other_commands() {
        let registry = Registry::new(vec![
            custom("goal", Origin::Project),
            custom("answer", Origin::User),
            custom("answer", Origin::PluginSkill("evil".into())),
        ]);
        assert_eq!(resolved(&registry, "goal"), "builtin:goal");
        assert_eq!(resolved(&registry, "answer"), "builtin:answer");
        // Still reachable under their prefix, never under the short name.
        assert_eq!(resolved(&registry, "project:goal"), "project:goal");
        let listed = registry.list();
        let project_goal = listed
            .iter()
            .find(|entry| entry["name"] == "project:goal")
            .expect("listed");
        assert_eq!(project_goal["short"], Value::Null);
        assert_eq!(project_goal["source"], "project");
    }

    #[test]
    fn an_mcp_prompt_is_a_command_of_its_own_name() {
        let prompt = |name: &str| crate::mcp::PromptCommand {
            name: name.into(),
            server: "docs".into(),
            prompt: "review".into(),
            description: "Review".into(),
            arguments: vec![crate::mcp::PromptArgument {
                name: "file".into(),
                description: Some("The file".into()),
                required: true,
            }],
        };
        let registry = Registry::new(vec![custom("mcp__docs__review", Origin::Project)])
            .with_prompts(vec![prompt("mcp__docs__review"), prompt("help")]);
        assert_eq!(
            resolved(&registry, "mcp__docs__review"),
            "mcp__docs__review"
        );
        // A built-in command is not overridden; the project's command keeps
        // its prefixed name.
        assert_eq!(resolved(&registry, "help"), "builtin:help");
        assert_eq!(
            resolved(&registry, "project:mcp__docs__review"),
            "project:mcp__docs__review"
        );
        let listed = registry.list();
        let entry = listed
            .iter()
            .find(|entry| entry["source"] == "mcp")
            .expect("listed");
        assert_eq!(entry["kind"], "prompt");
        assert_eq!(entry["server"], "docs");
        assert_eq!(entry["forms"][0]["usage"], "/mcp__docs__review <file>");
        assert_eq!(
            entry["forms"][0]["args"],
            json!([{"name": "file", "kind": "text", "required": true,
                    "description": "The file"}])
        );
        assert_eq!(
            listed
                .iter()
                .filter(|entry| entry["source"] == "mcp")
                .count(),
            1
        );
        let project = listed
            .iter()
            .find(|entry| entry["name"] == "project:mcp__docs__review")
            .expect("listed");
        assert_eq!(project["short"], Value::Null);
    }

    #[test]
    fn a_unique_command_is_reached_by_its_short_name() {
        let registry = Registry::new(vec![
            custom("deploy", Origin::User),
            custom("lint", Origin::PluginCommand("tools".into())),
        ]);
        assert_eq!(resolved(&registry, "deploy"), "user:deploy");
        assert_eq!(resolved(&registry, "user:deploy"), "user:deploy");
        assert_eq!(resolved(&registry, "lint"), "tools:lint");
        assert_eq!(resolved(&registry, "nothing"), "unknown_command");
    }

    #[test]
    fn same_named_commands_need_their_prefix() {
        let registry = Registry::new(vec![
            custom("check", Origin::User),
            custom("check", Origin::Project),
        ]);
        assert_eq!(resolved(&registry, "check"), "ambiguous_command");
        assert_eq!(resolved(&registry, "user:check"), "user:check");
        assert_eq!(resolved(&registry, "project:check"), "project:check");
        let conflicts = registry.conflicts();
        assert_eq!(conflicts[0]["name"], "check");
        assert_eq!(
            conflicts[0]["use"],
            json!(["/user:check", "/project:check"])
        );
        // Two commands with one full name can be called by neither.
        let twice = Registry::new(vec![
            custom("x", Origin::PluginCommand("p".into())),
            custom("x", Origin::PluginSkill("p".into())),
        ]);
        assert!(twice.blocked(&twice.customs[0]).is_some());
        assert_eq!(resolved(&twice, "p:x"), "ambiguous_command");
    }

    #[test]
    fn markdown_commands_are_read_with_their_front_matter() {
        let folder = tempfile::tempdir().expect("folder");
        std::fs::write(
            folder.path().join("fix.md"),
            "---\ndescription: \"Fix an issue\"\nargument-hint: <issue>\n---\nFix issue $ARGUMENTS now.\n",
        )
        .expect("write");
        std::fs::write(folder.path().join("plain.md"), "Explain the code.").expect("write");
        std::fs::write(folder.path().join("bad name.md"), "x").expect("write");
        std::fs::write(folder.path().join("notes.txt"), "x").expect("write");
        let found = markdown_commands(folder.path(), &Origin::Project);
        let names: Vec<&str> = found.iter().map(|custom| custom.name.as_str()).collect();
        assert_eq!(names, ["fix", "plain"]);
        assert_eq!(found[0].description, "Fix an issue");
        assert_eq!(found[0].hint.as_deref(), Some("<issue>"));
        assert_eq!(found[0].prompt("42").unwrap(), "Fix issue 42 now.");
        assert_eq!(found[1].prompt("").unwrap(), "Explain the code.");
        assert_eq!(
            found[1].prompt("main.rs").unwrap(),
            "Explain the code.\n\nARGUMENTS: main.rs"
        );
    }

    #[test]
    fn a_skill_is_called_as_a_prompt_naming_its_instructions() {
        let skill = Custom {
            name: "write".into(),
            origin: Origin::PluginSkill("notes".into()),
            description: String::new(),
            hint: None,
            path: PathBuf::from("/plugins/notes/skills/write/SKILL.md"),
        };
        let prompt = skill.prompt("a todo list").unwrap();
        assert!(prompt.starts_with("Use the skill notes:write"), "{prompt}");
        assert!(prompt.contains("SKILL.md"), "{prompt}");
        assert!(prompt.ends_with("ARGUMENTS: a todo list"), "{prompt}");
    }

    #[test]
    fn arguments_are_bound_to_the_form_they_call() {
        let goal = builtin_named("goal").unwrap();
        let (form, values) = choose(goal, "fix the tests").unwrap();
        assert_eq!(
            (form.sub, values),
            (None, vec!["fix the tests".to_string()])
        );
        assert_eq!(choose(goal, "status").unwrap().0.sub, Some("status"));
        assert_eq!(choose(goal, "").unwrap().0.sub, Some("status"));
        // A subcommand's name that begins free text is the free text.
        let (form, values) = choose(goal, "stop the leak in parser.rs").unwrap();
        assert_eq!(form.sub, None);
        assert_eq!(values, ["stop the leak in parser.rs"]);

        let notes = builtin_named("notes").unwrap();
        let (form, values) = choose(notes, "vote 3 up").unwrap();
        assert_eq!(
            (form.sub, values),
            (Some("vote"), vec!["3".into(), "up".into()])
        );
        assert!(choose(notes, "vote 3 sideways").is_err());
        assert!(choose(notes, "show x").is_err());
        assert!(choose(notes, "dance").is_err());
        let (_, values) = choose(notes, "edit 4 When x, do y").unwrap();
        assert_eq!(values, ["4", "When x, do y"]);

        let delete = builtin_named("delete").unwrap();
        assert!(choose(delete, "").is_err());
        assert!(choose(delete, "confirm").is_ok());
        assert!(choose(builtin_named("plan").unwrap(), "extra").is_err());
    }

    #[test]
    fn goal_options_come_before_the_objective() {
        let options =
            goal_options("--check \"cargo test -p x\" --rounds 5 --tokens 9000 fix --rounds")
                .unwrap();
        assert_eq!(options.check.as_deref(), Some("cargo test -p x"));
        assert_eq!(options.rounds, Some(5));
        assert_eq!(options.tokens, Some(9000));
        assert_eq!(options.objective, "fix --rounds");

        let plain = goal_options("fix the tests").unwrap();
        assert_eq!(
            (plain.check, plain.rounds, plain.tokens),
            (None, None, None)
        );
        assert_eq!(plain.objective, "fix the tests");

        assert!(goal_options("--rounds many fix").is_err());
        assert!(goal_options("--check \"cargo test fix").is_err());
        assert!(goal_options("--tokens 5").is_err());
    }

    #[test]
    fn mid_turn_use_follows_the_decided_rules() {
        let when = |name: &str, text: &str| {
            let command = builtin_named(name).unwrap();
            choose(command, text).unwrap().0.when
        };
        // Display commands and /translate at once.
        for (name, text) in [
            ("notes", ""),
            ("plan", ""),
            ("status", ""),
            ("issues", ""),
            ("goal", "status"),
            ("translate", "thinking on"),
            ("stop", ""),
        ] {
            assert_eq!(when(name, text), When::Now, "/{name} {text}");
        }
        for (name, text) in [
            ("model", "x"),
            ("effort", "high"),
            ("mode", "full"),
            ("plan", "on"),
            ("plan", "off"),
        ] {
            assert_eq!(when(name, text), When::NextRequest, "/{name} {text}");
        }
        for (name, text) in [
            ("new", ""),
            ("resume", ""),
            ("fork", ""),
            ("compact", ""),
            ("delete", "confirm"),
            ("goal", "fix it"),
        ] {
            assert_eq!(when(name, text), When::AfterTurn, "/{name} {text}");
        }
        assert!(allowed(When::AfterTurn, true).is_err());
        assert_eq!(
            allowed(When::AfterTurn, true).unwrap_err().when,
            Some(When::AfterTurn)
        );
        assert!(allowed(When::AfterTurn, false).is_ok());
        assert!(allowed(When::Now, true).is_ok());
        assert!(allowed(When::NextRequest, true).is_ok());
    }

    #[test]
    fn every_command_name_is_unique_and_every_form_well_made() {
        let mut names = std::collections::HashSet::new();
        for command in BUILTINS {
            assert!(plain_name(command.name), "{}", command.name);
            assert!(names.insert(command.name), "/{} twice", command.name);
            for alias in command.aliases {
                assert!(names.insert(alias), "/{alias} twice");
            }
            if let Some(bare) = command.bare {
                assert!(command.forms.iter().any(|form| form.sub == Some(bare)));
            }
            for form in command.forms {
                if form.runs == Runs::Turn {
                    assert_eq!(form.when, When::AfterTurn, "/{}", command.name);
                }
                let optional_seen = form.args.iter().position(|arg| !arg.required);
                if let Some(first) = optional_seen {
                    assert!(
                        form.args[first..].iter().all(|arg| !arg.required),
                        "/{}: a required argument after an optional one",
                        command.name
                    );
                }
            }
        }
    }

    /// Rule 6 of docs/evidence/slash-commands.md: every tool that keeps a
    /// record has a command that shows it.
    #[test]
    fn every_tool_that_keeps_a_record_has_a_command_that_shows_it() {
        let mut tools: Vec<String> = crate::agent_tools::specs(true)
            .into_iter()
            .chain(crate::agent_tools::specs(false))
            .chain([crate::goal::spec()])
            .map(|spec| spec.name)
            .collect();
        tools.push(crate::mcp::SEARCH.to_string());
        tools.push(crate::mcp::LIST_RESOURCES.to_string());
        tools.push(crate::mcp::LIST_TEMPLATES.to_string());
        tools.push(crate::mcp::READ_RESOURCE.to_string());
        tools.push(crate::company::DELEGATE.to_string());
        tools.push(crate::company::REVIEW.to_string());
        tools.sort();
        tools.dedup();
        for tool in &tools {
            let shown = SHOWN_BY.iter().any(|(name, _)| name == tool);
            let nothing = RECORDS_NOTHING.contains(&tool.as_str());
            assert!(
                shown ^ nothing,
                "the tool {tool} must be in exactly one of SHOWN_BY and RECORDS_NOTHING"
            );
        }
        for (tool, command) in SHOWN_BY {
            assert!(tools.iter().any(|name| name == tool), "{tool} is no tool");
            let command = builtin_named(command).expect("a built-in command");
            assert!(
                command.unavailable.is_none(),
                "/{} is unavailable",
                command.name
            );
        }
    }
}
