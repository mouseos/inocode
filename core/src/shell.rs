//! The `shell` and `job` tools: commands run in a pseudo terminal, so they
//! can be answered and driven like a person at a terminal. All output goes
//! to a log file; the model sees the start, the end, and the screen. Design
//! and approval: docs/evidence/harness-tools.md.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::{broadcast, watch};

use crate::conversation::ToolSpec;
use crate::store::SessionId;

pub(crate) const SHELL: &str = "shell";
pub(crate) const JOB: &str = "job";

const ROWS: u16 = 40;
const COLUMNS: u16 = 120;
const DEFAULT_WAIT: f64 = 30.0;
const MAX_WAIT: f64 = 600.0;
/// Lines shown from the start and from the end of a finished command.
const EDGE_LINES: usize = 20;
const LINE_CHARS: usize = 500;
/// Bytes of a command's output written to its log at most: a command that
/// never stops printing must not fill the disk. The screen keeps showing the
/// latest output.
const LOG_LIMIT: u64 = 64 * 1024 * 1024;
const LOG_CUT: &str = "\n[inocode: the log stops here at 64 MiB; the output went on]\n";

/// The tools; `sandboxed` is whether commands run confined in this turn.
pub(crate) fn specs(sandboxed: bool) -> Vec<ToolSpec> {
    let mut shell = "Run a command in a terminal in the project folder. It is never \
        killed for taking long: if it has not finished after wait_seconds, it keeps \
        running as a job and you get its screen. A finished command returns its exit \
        code, the first and last lines of its output, and the path of a log with all \
        of it; read the log for the rest. Use job to answer prompts, press keys, wait \
        longer, or stop it."
        .to_string();
    let mut properties = json!({
        "command": {"type": "string"},
        "wait_seconds": {"type": "number", "minimum": 0, "maximum": MAX_WAIT}
    });
    if sandboxed {
        shell.push_str(
            " The command can write only in the project folder and the temporary folder \
            in TMP. When the task needs a write elsewhere, do not try another way around \
            the denial: run the command again with outside_sandbox and a justification, \
            which asks for approval to run it outside the sandbox.",
        );
        properties["outside_sandbox"] = json!({
            "type": "boolean",
            "description": "Ask to run this command outside the sandbox. It runs only once approved."
        });
        properties["justification"] = json!({
            "type": "string",
            "description": "Why the command must run outside the sandbox."
        });
    }
    vec![
        ToolSpec {
            name: SHELL.into(),
            description: shell,
            parameters: json!({
                "type": "object",
                "required": ["command"],
                "properties": properties
            }),
        },
        ToolSpec {
            name: JOB.into(),
            description: "Work with a running command started by shell: type input, press \
                keys, wait for it, or stop it. Returns its screen while it runs and its \
                output once it has finished. Keys: Enter, Tab, Escape, Backspace, Delete, Up, \
                Down, Left, Right, Home, End, PageUp, PageDown, Space, Ctrl+<letter>."
                .into(),
            parameters: json!({
                "type": "object",
                "required": ["id"],
                "properties": {
                    "id": {"type": "integer"},
                    "input": {"type": "string", "description": "Text typed as is, before keys."},
                    "keys": {"type": "array", "items": {"type": "string"}},
                    "wait_seconds": {"type": "number", "minimum": 0, "maximum": MAX_WAIT},
                    "stop": {"type": "boolean"}
                }
            }),
        },
    ]
}

struct Job {
    id: u64,
    command: String,
    log: PathBuf,
    started: Instant,
    screen: Mutex<vt100::Parser>,
    writer: Mutex<Box<dyn Write + Send>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    /// The exit code and the run time, once the command has ended and all
    /// of its output is in the log.
    ended: watch::Sender<Option<(i64, Duration)>>,
    /// Whether the model has been shown how it ended.
    reported: Mutex<bool>,
    /// The terminal's output as it comes, for the screens that watch the
    /// job; sent while `screen` is locked, so a watcher that reads the
    /// screen and subscribes under that lock misses and repeats nothing.
    output: broadcast::Sender<Vec<u8>>,
}

/// The output chunks a watcher may fall behind by before it misses some.
const OUTPUT_KEPT: usize = 1024;

/// A job as the screens list it (docs/evidence/background-jobs.md).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct JobInfo {
    pub id: u64,
    pub command: String,
    pub running: bool,
    pub exit_code: Option<i64>,
    /// How long it ran, or has run so far.
    pub duration_ms: u64,
    pub log: String,
}

impl JobInfo {
    fn of(job: &Job) -> Self {
        let ended = *job.ended.borrow();
        Self {
            id: job.id,
            command: job.command.clone(),
            running: ended.is_none(),
            exit_code: ended.map(|(code, _)| code),
            duration_ms: ended
                .map_or_else(|| job.started.elapsed(), |(_, took)| took)
                .as_millis() as u64,
            log: job.log.display().to_string(),
        }
    }
}

/// A part of a job's log, without terminal control sequences.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct JobLog {
    pub job: JobInfo,
    pub text: String,
    /// Where `text` starts and the next part starts, in bytes of the log.
    pub offset: u64,
    pub next_offset: u64,
    /// The log's size when it was read.
    pub size: u64,
}

/// What a screen needs to follow a job: its screen now, as terminal text
/// that draws it, and the output from then on.
pub struct JobWatch {
    pub job: JobInfo,
    pub screen: String,
    pub output: broadcast::Receiver<Vec<u8>>,
    pub ended: watch::Receiver<Option<(i64, Duration)>>,
}

/// The jobs of a thread, oldest first.
pub(crate) fn list(logs: &Path, session: SessionId) -> Vec<JobInfo> {
    let Ok(registry) = registry().lock() else {
        return Vec::new();
    };
    registry
        .get(&(logs.to_path_buf(), session))
        .map(|jobs| jobs.jobs.iter().map(|job| JobInfo::of(job)).collect())
        .unwrap_or_default()
}

fn job_of(logs: &Path, session: SessionId, id: u64) -> Result<Arc<Job>, String> {
    find(&(logs.to_path_buf(), session), id).ok_or_else(|| format!("no job {id} in this thread"))
}

/// `limit` bytes of a job's log from `offset`, ending before a character
/// cut in two.
pub(crate) fn read_log(
    logs: &Path,
    session: SessionId,
    id: u64,
    offset: u64,
    limit: u64,
) -> Result<JobLog, String> {
    use std::io::Seek;
    let job = job_of(logs, session, id)?;
    let info = JobInfo::of(&job);
    let mut file = File::open(&job.log)
        .map_err(|error| format!("could not open {}: {error}", job.log.display()))?;
    let size = file
        .metadata()
        .map_err(|error| format!("could not read {}: {error}", job.log.display()))?
        .len();
    let offset = offset.min(size);
    file.seek(std::io::SeekFrom::Start(offset))
        .map_err(|error| format!("could not read {}: {error}", job.log.display()))?;
    let mut bytes = Vec::new();
    file.take(limit)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("could not read {}: {error}", job.log.display()))?;
    let whole = complete_utf8(&bytes);
    let next_offset = offset + whole as u64;
    Ok(JobLog {
        job: info,
        text: String::from_utf8_lossy(&bytes[..whole]).into_owned(),
        offset,
        next_offset,
        size,
    })
}

/// How many bytes of `bytes` leave no character cut at the end.
pub fn complete_utf8(bytes: &[u8]) -> usize {
    match std::str::from_utf8(bytes) {
        Ok(_) => bytes.len(),
        // An invalid sequence is kept, to be replaced; only an unfinished
        // one at the end waits for the rest.
        Err(error) if error.error_len().is_none() => error.valid_up_to(),
        Err(_) => bytes.len(),
    }
}

/// Types `bytes` into a running job.
pub(crate) fn write_to(
    logs: &Path,
    session: SessionId,
    id: u64,
    bytes: &[u8],
) -> Result<(), String> {
    let job = job_of(logs, session, id)?;
    if job.ended.borrow().is_some() {
        return Err(format!("job {id} has already ended"));
    }
    let mut writer = job.writer.lock().map_err(|_| "the job is unusable")?;
    writer
        .write_all(bytes)
        .and_then(|()| writer.flush())
        .map_err(|error| format!("could not send to job {id}: {error}"))
}

/// The bytes of `input` and then of each key, by the names the `job` tool
/// takes.
pub(crate) fn typed(input: Option<&str>, keys: &[String]) -> Result<Vec<u8>, String> {
    let mut bytes = input.unwrap_or_default().as_bytes().to_vec();
    for key in keys {
        bytes.extend(key_bytes(key)?);
    }
    Ok(bytes)
}

/// Stops a job; `false` when it had already ended.
pub(crate) fn stop_job(logs: &Path, session: SessionId, id: u64) -> Result<bool, String> {
    let job = job_of(logs, session, id)?;
    if job.ended.borrow().is_some() {
        return Ok(false);
    }
    let mut killer = job.killer.lock().map_err(|_| "the job is unusable")?;
    let killed = killer.kill();
    // portable-pty 0.9.0 reports a Windows kill that succeeded as an error
    // and one that failed as success, so there the result says nothing.
    if cfg!(not(windows)) {
        killed.map_err(|error| format!("could not stop job {id}: {error}"))?;
    }
    Ok(true)
}

pub(crate) fn watch_job(logs: &Path, session: SessionId, id: u64) -> Result<JobWatch, String> {
    let job = job_of(logs, session, id)?;
    let screen = job.screen.lock().map_err(|_| "the job is unusable")?;
    let output = job.output.subscribe();
    let drawn = screen.screen().contents_formatted();
    drop(screen);
    Ok(JobWatch {
        job: JobInfo::of(&job),
        screen: drawn,
        output,
        ended: job.ended.subscribe(),
    })
}

/// A command the person runs from the input box, outside the sandbox
/// (docs/evidence/person-shell.md).
pub(crate) struct PersonJob(Arc<Job>);

impl PersonJob {
    pub(crate) fn start(place: Place<'_>, command: &str) -> Result<Self, String> {
        let folder = place
            .folder
            .filter(|folder| folder.is_dir())
            .ok_or("commands need a project folder, and this thread has none")?;
        let job = start(
            (place.logs, place.session),
            folder,
            command.to_string(),
            false,
            place.activity,
        )?;
        Ok(Self(job))
    }

    pub(crate) fn id(&self) -> u64 {
        self.0.id
    }

    /// Waits for the command to end, or stops it when `cancel` is raised;
    /// gives how it ended as the model is shown a job's end, and whether it
    /// was stopped.
    pub(crate) async fn finish(self, mut cancel: watch::Receiver<bool>) -> (String, bool) {
        let job = self.0;
        let mut ended = job.ended.subscribe();
        let stopped = tokio::select! {
            () = async { drop(ended.wait_for(Option::is_some).await) } => false,
            () = cancelled(&mut cancel) => true,
        };
        if stopped && job.ended.borrow().is_none() {
            if let Ok(mut killer) = job.killer.lock() {
                let _ = killer.kill();
            }
            let mut ended = job.ended.subscribe();
            let _ = tokio::time::timeout(Duration::from_secs(5), async {
                drop(ended.wait_for(Option::is_some).await)
            })
            .await;
        }
        if let Ok(mut reported) = job.reported.lock() {
            *reported = true;
        }
        let state = *job.ended.borrow();
        let text = match state {
            Some((code, took)) => finished(&job, code, took),
            None => running(&job),
        };
        (text, stopped)
    }
}

/// Tells the thread's watchers what its jobs now are.
fn announce(key: &Key, activity: &broadcast::Sender<(SessionId, crate::Activity)>) {
    let jobs = list(&key.0, key.1);
    let _ = activity.send((key.1, crate::Activity::Jobs(jobs)));
}

/// Jobs are kept per log folder (one per database) and thread, so threads
/// of different databases in one process never see each other's jobs.
type Key = (PathBuf, SessionId);

#[derive(Default)]
struct Jobs {
    next: u64,
    jobs: Vec<Arc<Job>>,
}

fn registry() -> &'static Mutex<HashMap<Key, Jobs>> {
    static JOBS: OnceLock<Mutex<HashMap<Key, Jobs>>> = OnceLock::new();
    JOBS.get_or_init(Default::default)
}

fn find(key: &Key, id: u64) -> Option<Arc<Job>> {
    let registry = registry().lock().ok()?;
    registry
        .get(key)?
        .jobs
        .iter()
        .find(|job| job.id == id)
        .cloned()
}

/// What a tool call needs from the thread.
pub(crate) struct Place<'a> {
    pub session: SessionId,
    /// The project folder, where commands run.
    pub folder: Option<&'a Path>,
    /// Where logs are written; it also tells the database apart.
    pub logs: PathBuf,
    pub cancel: watch::Receiver<bool>,
    /// Whether commands run confined; a call that asked to run outside the
    /// sandbox reaches [`shell`] only once approved.
    pub sandboxed: bool,
    pub activity: broadcast::Sender<(SessionId, crate::Activity)>,
}

#[derive(Deserialize)]
pub(crate) struct ShellArguments {
    pub command: String,
    #[serde(default)]
    wait_seconds: Option<f64>,
    #[serde(default)]
    pub outside_sandbox: bool,
}

/// The `shell` arguments, when they parse.
pub(crate) fn shell_arguments(arguments: &str) -> Option<ShellArguments> {
    serde_json::from_str(arguments).ok()
}

/// A program whose runs only read, with the flags known not to write,
/// run another program or open a pager. A flag in `flags` ending in `*`
/// takes any rest (`--format=*`, which also allows `--format` with its value
/// as the next word); `short` lists the letters that may be
/// combined after one `-`. `positional` says whether words that are not
/// flags may follow.
struct Reader {
    name: &'static str,
    short: &'static str,
    flags: &'static [&'static str],
    positional: bool,
}

const READERS: &[Reader] = &[
    Reader {
        name: "ls",
        short: "1aAlhRrtSdFiGgnopsuvx",
        flags: &[
            "--all",
            "--almost-all",
            "--human-readable",
            "--recursive",
            "--reverse",
            "--color*",
            "--classify",
            "--directory",
            "--sort=*",
            "--time=*",
            "--full-time",
            "--group-directories-first",
            "--inode",
            "--size",
        ],
        positional: true,
    },
    Reader {
        name: "dir",
        short: "1aAlhRrtSdF",
        flags: &["--all", "--human-readable", "--recursive"],
        positional: true,
    },
    Reader {
        name: "pwd",
        short: "LP",
        flags: &[],
        positional: false,
    },
    Reader {
        name: "cat",
        short: "nAbeEsTtv",
        flags: &[
            "--number",
            "--show-all",
            "--number-nonblank",
            "--squeeze-blank",
            "--show-ends",
            "--show-tabs",
        ],
        positional: true,
    },
    Reader {
        name: "head",
        short: "nc0123456789qvz",
        flags: &["--lines=*", "--bytes=*", "--quiet", "--silent", "--verbose"],
        positional: true,
    },
    Reader {
        name: "tail",
        short: "nc0123456789qvz",
        flags: &["--lines=*", "--bytes=*", "--quiet", "--silent", "--verbose"],
        positional: true,
    },
    Reader {
        name: "wc",
        short: "lwcmL",
        flags: &[
            "--lines",
            "--words",
            "--bytes",
            "--chars",
            "--max-line-length",
        ],
        positional: true,
    },
    Reader {
        name: "grep",
        short: "rRinvwxclLoqsHhEFGzZ0123456789ABCe",
        flags: &[
            "--recursive",
            "--ignore-case",
            "--line-number",
            "--invert-match",
            "--word-regexp",
            "--line-regexp",
            "--count",
            "--files-with-matches",
            "--files-without-match",
            "--only-matching",
            "--quiet",
            "--no-messages",
            "--with-filename",
            "--no-filename",
            "--extended-regexp",
            "--fixed-strings",
            "--include=*",
            "--exclude=*",
            "--exclude-dir=*",
            "--color*",
            "--context=*",
            "--after-context=*",
            "--before-context=*",
            "--regexp=*",
            "--max-count=*",
            "-A*",
            "-B*",
            "-C*",
            "-m*",
            "-e*",
        ],
        positional: true,
    },
    Reader {
        name: "rg",
        short: "iSFwxvcClLnNHIuo0123456789ABCetgsM",
        flags: &[
            "--ignore-case",
            "--smart-case",
            "--case-sensitive",
            "--fixed-strings",
            "--word-regexp",
            "--line-regexp",
            "--invert-match",
            "--count",
            "--count-matches",
            "--files",
            "--files-with-matches",
            "--files-without-match",
            "--line-number",
            "--no-line-number",
            "--with-filename",
            "--no-filename",
            "--hidden",
            "--no-ignore",
            "--no-ignore-vcs",
            "--follow",
            "--glob=*",
            "--iglob=*",
            "--type=*",
            "--type-not=*",
            "--type-list",
            "--max-count=*",
            "--max-depth=*",
            "--max-columns=*",
            "--context=*",
            "--after-context=*",
            "--before-context=*",
            "--regexp=*",
            "--multiline",
            "--only-matching",
            "--json",
            "--color=*",
            "--heading",
            "--no-heading",
            "--trim",
            "--sort=*",
            "--sortr=*",
            "--stats",
            "--column",
            "--vimgrep",
            "--unrestricted",
            "-A*",
            "-B*",
            "-C*",
            "-m*",
            "-g*",
            "-t*",
            "-T*",
            "-e*",
            "-M*",
        ],
        positional: true,
    },
    Reader {
        name: "find",
        short: "LHP",
        flags: &[
            "-name",
            "-iname",
            "-path",
            "-ipath",
            "-wholename",
            "-iwholename",
            "-regex",
            "-iregex",
            "-type",
            "-xtype",
            "-maxdepth",
            "-mindepth",
            "-size",
            "-mtime",
            "-mmin",
            "-atime",
            "-amin",
            "-ctime",
            "-cmin",
            "-newer",
            "-empty",
            "-print",
            "-print0",
            "-printf",
            "-not",
            "-and",
            "-or",
            "-a",
            "-o",
            "-prune",
            "-follow",
            "-depth",
            "-xdev",
            "-mount",
            "-user",
            "-group",
            "-perm",
            "-readable",
            "-writable",
            "-executable",
            "-samefile",
            "-links",
            "-inum",
            "-true",
            "-false",
            "-ls",
        ],
        positional: true,
    },
    Reader {
        name: "fd",
        short: "HIiFsgLadpelt0123456789",
        flags: &[
            "--hidden",
            "--no-ignore",
            "--ignore-case",
            "--case-sensitive",
            "--fixed-strings",
            "--glob",
            "--follow",
            "--absolute-path",
            "--full-path",
            "--type=*",
            "--extension=*",
            "--max-depth=*",
            "--min-depth=*",
            "--exact-depth=*",
            "--exclude=*",
            "--size=*",
            "--changed-within=*",
            "--changed-before=*",
            "--max-results=*",
            "--color=*",
            "--list-details",
            "-t*",
            "-e*",
            "-d*",
            "-E*",
        ],
        positional: true,
    },
    Reader {
        name: "tree",
        short: "adfFLilpshDCnJ0123456789",
        flags: &[
            "--gitignore",
            "--noreport",
            "--dirsfirst",
            "-I*",
            "-P*",
            "-L*",
        ],
        positional: true,
    },
    Reader {
        name: "stat",
        short: "Lt",
        flags: &["--dereference", "--terse"],
        positional: true,
    },
    Reader {
        name: "file",
        short: "biLzk",
        flags: &["--brief", "--mime", "--mime-type", "--dereference"],
        positional: true,
    },
    Reader {
        name: "which",
        short: "a",
        flags: &["--all"],
        positional: true,
    },
    Reader {
        name: "du",
        short: "ahsckmxLd0123456789",
        flags: &[
            "--all",
            "--human-readable",
            "--summarize",
            "--total",
            "--max-depth=*",
            "--apparent-size",
            "-d*",
        ],
        positional: true,
    },
    Reader {
        name: "df",
        short: "ahHkTiPl",
        flags: &[
            "--all",
            "--human-readable",
            "--si",
            "--print-type",
            "--inodes",
            "--local",
        ],
        positional: true,
    },
    Reader {
        name: "date",
        short: "uRI",
        flags: &[
            "--utc",
            "--universal",
            "--rfc-email",
            "--iso-8601*",
            "--rfc-3339=*",
            "-I*",
        ],
        positional: false,
    },
    Reader {
        name: "whoami",
        short: "",
        flags: &[],
        positional: false,
    },
];

/// PowerShell's reading cmdlets (and their usual aliases), whose
/// parameters do not write files or run programs. `where` is
/// `Where-Object` and `type` is `Get-Content` there.
const POWERSHELL_READERS: &[&str] = &[
    "get-childitem",
    "gci",
    "get-content",
    "gc",
    "get-location",
    "gl",
    "get-item",
    "gi",
    "select-string",
    "sls",
    "test-path",
    "resolve-path",
    "rvpa",
    "get-command",
    "gcm",
    "where",
    "type",
];

/// Git's reading subcommands with their flags. Options before the
/// subcommand (`-c`, `--exec-path`, ...) are never taken as reading.
const GIT_READERS: &[Reader] = &[
    Reader {
        name: "status",
        short: "sbvzu",
        flags: &[
            "--short",
            "--branch",
            "--porcelain*",
            "--long",
            "--verbose",
            "--untracked-files*",
            "--ignored*",
            "--show-stash",
            "--no-renames",
            "--ahead-behind",
            "--no-ahead-behind",
            "-u*",
        ],
        positional: true,
    },
    Reader {
        name: "diff",
        short: "pubwRWM0123456789",
        flags: DIFF_FLAGS,
        positional: true,
    },
    Reader {
        name: "log",
        short: "pnuiEFwM0123456789",
        flags: LOG_FLAGS,
        positional: true,
    },
    Reader {
        name: "show",
        short: "puswM0123456789",
        flags: LOG_FLAGS,
        positional: true,
    },
    Reader {
        name: "blame",
        short: "wesltcfnbM",
        flags: &[
            "--porcelain",
            "--line-porcelain",
            "--root",
            "--show-name",
            "--show-number",
            "--show-email",
            "--date=*",
            "-L*",
            "-M*",
            "-C*",
        ],
        positional: true,
    },
    Reader {
        name: "rev-parse",
        short: "q",
        flags: &[
            "--show-toplevel",
            "--show-prefix",
            "--show-cdup",
            "--abbrev-ref*",
            "--short*",
            "--verify",
            "--quiet",
            "--git-dir",
            "--absolute-git-dir",
            "--git-common-dir",
            "--is-inside-work-tree",
            "--is-inside-git-dir",
            "--is-bare-repository",
            "--is-shallow-repository",
            "--symbolic",
            "--symbolic-full-name",
            "--all",
            "--branches*",
            "--tags*",
            "--remotes*",
        ],
        positional: true,
    },
    Reader {
        name: "ls-files",
        short: "cdmoiskutvfz",
        flags: &[
            "--cached",
            "--deleted",
            "--modified",
            "--others",
            "--ignored",
            "--stage",
            "--killed",
            "--unmerged",
            "--exclude=*",
            "--exclude-standard",
            "--full-name",
            "--directory",
            "--error-unmatch",
            "--recurse-submodules",
            "-x*",
        ],
        positional: true,
    },
    Reader {
        name: "grep",
        short: "nivwlLcEFPhHoqzpW0123456789ABCe",
        flags: &[
            "--line-number",
            "--ignore-case",
            "--invert-match",
            "--word-regexp",
            "--files-with-matches",
            "--files-without-match",
            "--count",
            "--extended-regexp",
            "--fixed-strings",
            "--perl-regexp",
            "--cached",
            "--untracked",
            "--no-index",
            "--heading",
            "--break",
            "--show-function",
            "--function-context",
            "--only-matching",
            "--max-depth=*",
            "--max-count=*",
            "--context=*",
            "--after-context=*",
            "--before-context=*",
            "--and",
            "--or",
            "--not",
            "--all-match",
            "--name-only",
            "--no-color",
            "--color*",
            "-A*",
            "-B*",
            "-C*",
            "-e*",
            "-m*",
        ],
        positional: true,
    },
];

const DIFF_FLAGS: &[&str] = &[
    "--stat*",
    "--numstat",
    "--shortstat",
    "--dirstat*",
    "--name-only",
    "--name-status",
    "--cached",
    "--staged",
    "--patch",
    "--no-patch",
    "--unified=*",
    "--ignore-all-space",
    "--ignore-space-change",
    "--ignore-blank-lines",
    "--color*",
    "--no-color",
    "--word-diff*",
    "--check",
    "--summary",
    "--find-renames*",
    "--find-copies*",
    "--no-renames",
    "--minimal",
    "--patience",
    "--histogram",
    "--relative*",
    "--diff-filter=*",
    "--merge-base",
    "--no-index",
    "--no-ext-diff",
    "--no-textconv",
    "--raw",
    "--abbrev*",
    "--full-index",
    "--binary",
    "--quiet",
    "--exit-code",
    "-U*",
    "-M*",
    "--",
];

const LOG_FLAGS: &[&str] = &[
    "--oneline",
    "--max-count=*",
    "--skip=*",
    "--graph",
    "--decorate*",
    "--no-decorate",
    "--all",
    "--branches*",
    "--tags*",
    "--remotes*",
    "--format=*",
    "--pretty*",
    "--author=*",
    "--committer=*",
    "--since=*",
    "--until=*",
    "--after=*",
    "--before=*",
    "--grep=*",
    "--follow",
    "--name-only",
    "--name-status",
    "--reverse",
    "--first-parent",
    "--merges",
    "--no-merges",
    "--abbrev-commit",
    "--no-abbrev-commit",
    "--date=*",
    "--relative-date",
    "--color*",
    "--no-color",
    "--stat*",
    "--numstat",
    "--shortstat",
    "--patch",
    "--no-patch",
    "--left-right",
    "--cherry-pick",
    "--no-walk*",
    "--topo-order",
    "--date-order",
    "--ignore-case",
    "--regexp-ignore-case",
    "--all-match",
    "--no-ext-diff",
    "--no-textconv",
    "--word-diff*",
    "--unified=*",
    "--diff-filter=*",
    "--find-renames*",
    "--summary",
    "--raw",
    "--full-history",
    "--simplify-by-decoration",
    "--show-signature",
    "-L*",
    "-U*",
    "-S*",
    "-G*",
    "--",
];

/// The words of a command with its quotes removed, as the shell that runs
/// it splits them: PowerShell keeps a backslash as a letter, sh takes it as an
/// escape. `None` when a quote is not closed.
fn words(command: &str, powershell: bool) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut word: Option<String> = None;
    let mut chars = command.chars();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {
                words.extend(word.take());
            }
            '\'' => {
                let text = word.get_or_insert_with(String::new);
                loop {
                    match chars.next()? {
                        '\'' => break,
                        c => text.push(c),
                    }
                }
            }
            '"' => {
                let text = word.get_or_insert_with(String::new);
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' if !powershell => text.push(chars.next()?),
                        c => text.push(c),
                    }
                }
            }
            '\\' if !powershell => word.get_or_insert_with(String::new).push(chars.next()?),
            c => word.get_or_insert_with(String::new).push(c),
        }
    }
    words.extend(word);
    Some(words)
}

/// Whether every word after the program is allowed for `reader`.
fn allowed(reader: &Reader, arguments: &[String]) -> bool {
    arguments.iter().all(|word| {
        let flag = word.starts_with('-') && word.len() > 1;
        if !flag {
            return reader.positional;
        }
        let listed = reader
            .flags
            .iter()
            .any(|allowed| match allowed.strip_suffix('*') {
                Some(prefix) => {
                    word.starts_with(prefix) || prefix.strip_suffix('=') == Some(word.as_str())
                }
                None => word == allowed,
            });
        let short = !word.starts_with("--")
            && word[1..]
                .chars()
                .all(|letter| reader.short.contains(letter));
        listed || short
    })
}

/// Whether `command` only reads: one plain command (no chaining, pipes,
/// redirections, substitutions, groups or variables) whose program, and
/// for git its subcommand, is on a list of reading commands, with only
/// flags known not to write, run another program or open a pager.
/// Anything else counts as a change (docs/evidence/approval-modes.md).
pub(crate) fn reads_only(command: &str) -> bool {
    reads_only_in(command, cfg!(windows))
}

fn reads_only_in(command: &str, powershell: bool) -> bool {
    if command.chars().any(|c| {
        matches!(
            c,
            ';' | '&' | '|' | '>' | '<' | '`' | '$' | '(' | ')' | '{' | '}' | '@' | '\n' | '\r'
        )
    }) {
        return false;
    }
    let Some(words) = words(command, powershell) else {
        return false;
    };
    let Some((program, arguments)) = words.split_first() else {
        return false;
    };
    let program = program.to_ascii_lowercase();
    if program == "git" {
        let Some((sub, arguments)) = arguments.split_first() else {
            return false;
        };
        return GIT_READERS
            .iter()
            .find(|reader| reader.name == sub)
            .is_some_and(|reader| allowed(reader, arguments));
    }
    if powershell && POWERSHELL_READERS.contains(&program.as_str()) {
        return true;
    }
    READERS
        .iter()
        .find(|reader| reader.name == program)
        .is_some_and(|reader| allowed(reader, arguments))
}

/// Whether a `job` call types into its command, which is a change like a
/// command of its own.
pub(crate) fn job_types(arguments: &str) -> bool {
    parse::<JobArguments>(arguments).map_or(true, |arguments| {
        arguments.input.is_some_and(|input| !input.is_empty()) || !arguments.keys.is_empty()
    })
}

#[derive(Deserialize)]
struct JobArguments {
    id: u64,
    #[serde(default)]
    input: Option<String>,
    #[serde(default)]
    keys: Vec<String>,
    #[serde(default)]
    wait_seconds: Option<f64>,
    #[serde(default)]
    stop: bool,
}

fn parse<T: for<'de> Deserialize<'de>>(arguments: &str) -> Result<T, String> {
    serde_json::from_str(arguments).map_err(|error| format!("invalid arguments: {error}"))
}

fn wait_time(seconds: Option<f64>, default: f64) -> Duration {
    Duration::from_secs_f64(seconds.unwrap_or(default).clamp(0.0, MAX_WAIT))
}

pub(crate) async fn shell(place: Place<'_>, arguments: &str) -> Result<String, String> {
    let arguments: ShellArguments = parse(arguments)?;
    if arguments.command.trim().is_empty() {
        return Err("command must not be empty".into());
    }
    let folder = place
        .folder
        .filter(|folder| folder.is_dir())
        .ok_or("commands need a project folder, and this thread has none")?;
    let confined = place.sandboxed && !arguments.outside_sandbox;
    let job = start(
        (place.logs, place.session),
        folder,
        arguments.command,
        confined,
        place.activity,
    )?;
    let wait = wait_time(arguments.wait_seconds, DEFAULT_WAIT);
    Ok(settle(&job, wait, place.cancel).await)
}

pub(crate) async fn job(place: Place<'_>, arguments: &str) -> Result<String, String> {
    let arguments: JobArguments = parse(arguments)?;
    let job = find(&(place.logs, place.session), arguments.id)
        .ok_or_else(|| format!("no job {} in this thread", arguments.id))?;
    let mut bytes = arguments.input.unwrap_or_default().into_bytes();
    for key in &arguments.keys {
        bytes.extend(key_bytes(key)?);
    }
    if !bytes.is_empty() {
        if job.ended.borrow().is_some() {
            return Err(format!("job {} has already ended", job.id));
        }
        let mut writer = job.writer.lock().map_err(|_| "the job is unusable")?;
        writer
            .write_all(&bytes)
            .and_then(|()| writer.flush())
            .map_err(|error| format!("could not send to job {}: {error}", job.id))?;
    }
    if arguments.stop && job.ended.borrow().is_none() {
        let mut killer = job.killer.lock().map_err(|_| "the job is unusable")?;
        killer
            .kill()
            .map_err(|error| format!("could not stop job {}: {error}", job.id))?;
    }
    let default = if arguments.stop || !bytes.is_empty() {
        2.0
    } else {
        DEFAULT_WAIT
    };
    let wait = wait_time(arguments.wait_seconds, default);
    Ok(settle(&job, wait, place.cancel).await)
}

/// The jobs of a thread the model has not yet seen end, and the ones still
/// running, for the state shown with each request.
pub(crate) fn render_jobs(logs: &Path, session: SessionId) -> Option<String> {
    let registry = registry().lock().ok()?;
    let jobs = &registry.get(&(logs.to_path_buf(), session))?.jobs;
    let mut lines = Vec::new();
    for job in jobs {
        let reported = job.reported.lock().map(|r| *r).unwrap_or(true);
        match *job.ended.borrow() {
            Some((code, _)) if !reported => lines.push(format!(
                "- job {} ended with exit code {code}: {} (call job with its id for the output)",
                job.id,
                cut(&job.command, 200)
            )),
            Some(_) => {}
            None => lines.push(format!(
                "- job {} is running: {}",
                job.id,
                cut(&job.command, 200)
            )),
        }
    }
    (!lines.is_empty()).then(|| format!("Jobs:\n{}\n", lines.join("\n")))
}

/// Stops every command a thread still runs.
pub(crate) fn stop_all(logs: &Path, session: SessionId) {
    let Ok(registry) = registry().lock() else {
        return;
    };
    let Some(jobs) = registry.get(&(logs.to_path_buf(), session)) else {
        return;
    };
    for job in &jobs.jobs {
        if job.ended.borrow().is_none() {
            if let Ok(mut killer) = job.killer.lock() {
                let _ = killer.kill();
            }
        }
    }
}

fn start(
    key: Key,
    folder: &Path,
    command: String,
    confined: bool,
    activity: broadcast::Sender<(SessionId, crate::Activity)>,
) -> Result<Arc<Job>, String> {
    let id = {
        let mut registry = registry().lock().map_err(|_| "the job list is unusable")?;
        let jobs = registry.entry(key.clone()).or_default();
        jobs.next += 1;
        jobs.next
    };
    let logs = key.0.join(format!("thread-{}", key.1 .0));
    std::fs::create_dir_all(&logs)
        .map_err(|error| format!("could not create {}: {error}", logs.display()))?;
    let (log, file) = new_log(&logs, id)?;

    let pty = native_pty_system();
    let pair = pty
        .openpty(PtySize {
            rows: ROWS,
            cols: COLUMNS,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|error| format!("could not open a terminal: {error:#}"))?;
    let temp = logs.join("tmp");
    std::fs::create_dir_all(&temp)
        .map_err(|error| format!("could not create {}: {error}", temp.display()))?;
    let roots = crate::sandbox::Roots {
        folder: folder.to_path_buf(),
        temp: temp.clone(),
    };
    let argv = if confined {
        crate::sandbox::confine(&shell_command(&command), &roots)?
    } else {
        shell_command(&command)
    };
    let mut builder = CommandBuilder::from_argv(argv.into_iter().map(Into::into).collect());
    builder.cwd(folder);
    builder.env("TERM", "xterm-256color");
    // A pager would wait for keys, and `less` runs commands typed into it.
    for (name, value) in [("GIT_PAGER", "cat"), ("PAGER", "cat"), ("LESSSECURE", "1")] {
        builder.env(name, value);
    }
    for name in ["TMP", "TEMP", "TMPDIR"] {
        builder.env(name, &temp);
    }
    let mut child = pair
        .slave
        .spawn_command(builder)
        .map_err(|error| format!("could not start the command: {error:#}"))?;
    drop(pair.slave);
    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|error| format!("could not read the terminal: {error:#}"))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|error| format!("could not write to the terminal: {error:#}"))?;
    let (ended, _) = watch::channel(None);
    let job = Arc::new(Job {
        id,
        command,
        log,
        started: Instant::now(),
        screen: Mutex::new(vt100::Parser::new(nonzero(ROWS), nonzero(COLUMNS), 0)),
        writer: Mutex::new(writer),
        killer: Mutex::new(child.clone_killer()),
        ended,
        reported: Mutex::new(false),
        output: broadcast::channel(OUTPUT_KEPT).0,
    });
    registry()
        .lock()
        .map_err(|_| "the job list is unusable")?
        .entry(key.clone())
        .or_default()
        .jobs
        .push(Arc::clone(&job));
    announce(&key, &activity);

    let copier = {
        let job = Arc::clone(&job);
        std::thread::spawn(move || copy(reader, file, &job))
    };
    let master: Box<dyn MasterPty + Send> = pair.master;
    let waiter = Arc::clone(&job);
    std::thread::spawn(move || {
        let code = child
            .wait()
            .map(|status| i64::from(status.exit_code()))
            .unwrap_or(-1);
        // The terminal stays open until the command ends; closing it then
        // lets the copier see the end of the output on every platform.
        std::thread::sleep(Duration::from_millis(100));
        drop(master);
        let _ = copier.join();
        waiter
            .ended
            .send_replace(Some((code, waiter.started.elapsed())));
        announce(&key, &activity);
    });
    Ok(job)
}

/// A new log file for job `id`. Job numbers start again with each process,
/// so the name also carries the start time, and an existing log is never
/// replaced: the logs named in stored conversations keep their output.
fn new_log(logs: &Path, id: u64) -> Result<(PathBuf, File), String> {
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S%.3f");
    for attempt in 0u32.. {
        let name = match attempt {
            0 => format!("job-{id}-{stamp}.log"),
            n => format!("job-{id}-{stamp}-{n}.log"),
        };
        let log = logs.join(name);
        match File::options().write(true).create_new(true).open(&log) {
            Ok(file) => return Ok((log, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(format!("could not create {}: {error}", log.display())),
        }
    }
    unreachable!("some name is free")
}

/// The shell commands run in, as the model is told.
#[cfg(windows)]
pub(crate) const COMMAND_SHELL: &str =
    "Windows PowerShell (powershell.exe); use PowerShell commands \
    and syntax, not Unix ones";

#[cfg(not(windows))]
pub(crate) const COMMAND_SHELL: &str = "sh (sh -c); use POSIX shell syntax";

#[cfg(windows)]
fn shell_command(command: &str) -> Vec<String> {
    [
        "powershell.exe",
        "-NoLogo",
        "-NoProfile",
        "-Command",
        command,
    ]
    .map(String::from)
    .to_vec()
}

#[cfg(not(windows))]
fn shell_command(command: &str) -> Vec<String> {
    ["sh", "-c", command].map(String::from).to_vec()
}

/// Copies the terminal's output to the screen and, without terminal
/// control sequences, to the log.
fn copy(mut reader: Box<dyn Read + Send>, file: File, job: &Job) {
    let mut log = CappedLog {
        out: strip_ansi_escapes::Writer::new(file),
        written: 0,
        limit: LOG_LIMIT,
    };
    let mut buffer = [0u8; 8192];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                let chunk = &buffer[..read];
                let cursor = job.screen.lock().ok().map(|mut screen| {
                    screen.process(chunk);
                    let _ = job.output.send(chunk.to_vec());
                    screen.screen().cursor_position()
                });
                // Windows' pseudo console asks where the cursor is and
                // waits for the answer before it runs the command.
                if let Some((row, column)) = cursor.filter(|_| asks_cursor(chunk)) {
                    if let Ok(mut writer) = job.writer.lock() {
                        let answer = format!("[{};{}R", row + 1, column + 1);
                        let _ = writer.write_all(answer.as_bytes());
                        let _ = writer.flush();
                    }
                }
                log.write(chunk);
            }
        }
    }
    let _ = log.out.flush();
}

/// A log that takes `limit` bytes at most, then notes that it stopped.
struct CappedLog<W: Write> {
    out: W,
    written: u64,
    limit: u64,
}

impl<W: Write> CappedLog<W> {
    fn write(&mut self, chunk: &[u8]) {
        if self.written >= self.limit {
            return;
        }
        self.written += chunk.len() as u64;
        let _ = self.out.write_all(chunk);
        if self.written >= self.limit {
            let _ = self.out.write_all(LOG_CUT.as_bytes());
        }
    }
}

fn asks_cursor(chunk: &[u8]) -> bool {
    chunk.windows(4).any(|window| window == b"[6n")
}

/// Waits for the job to end, at most `wait` and never past a cancelled
/// turn, then describes it.
async fn settle(job: &Job, wait: Duration, mut cancel: watch::Receiver<bool>) -> String {
    let mut ended = job.ended.subscribe();
    tokio::select! {
        _ = tokio::time::timeout(wait, ended.wait_for(Option::is_some)) => {}
        () = cancelled(&mut cancel) => {}
    }
    let state = *job.ended.borrow();
    match state {
        Some((code, took)) => {
            if let Ok(mut reported) = job.reported.lock() {
                *reported = true;
            }
            finished(job, code, took)
        }
        None => running(job),
    }
}

async fn cancelled(cancel: &mut watch::Receiver<bool>) {
    loop {
        if *cancel.borrow_and_update() {
            return;
        }
        if cancel.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

fn finished(job: &Job, code: i64, took: Duration) -> String {
    let text = std::fs::read(&job.log)
        .map(|bytes| {
            String::from_utf8_lossy(&bytes)
                .replace("\r\n", "\n")
                .replace('\r', "\n")
        })
        .unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    let mut answer = format!(
        "Job {} ended with exit code {code} after {:.1}s. Its {} lines of output are in {}.",
        job.id,
        took.as_secs_f64(),
        lines.len(),
        job.log.display()
    );
    answer.push_str(&edges(&lines));
    answer
}

/// The whole output when it is short; otherwise its first and last lines.
fn edges(lines: &[&str]) -> String {
    let show = |lines: &[&str]| -> String {
        lines
            .iter()
            .map(|line| cut(line, LINE_CHARS))
            .collect::<Vec<_>>()
            .join("\n")
    };
    if lines.is_empty() {
        return String::new();
    }
    if lines.len() <= EDGE_LINES * 2 {
        return format!("\n{}", show(lines));
    }
    format!(
        "\n{}\n… {} lines not shown …\n{}",
        show(&lines[..EDGE_LINES]),
        lines.len() - EDGE_LINES * 2,
        show(&lines[lines.len() - EDGE_LINES..])
    )
}

fn running(job: &Job) -> String {
    let screen = job
        .screen
        .lock()
        .map(|parser| parser.screen().contents())
        .unwrap_or_default();
    let screen = screen.trim_end();
    format!(
        "Job {} is still running after {:.1}s (output so far in {}). Its screen:\n{}",
        job.id,
        job.started.elapsed().as_secs_f64(),
        job.log.display(),
        if screen.is_empty() { "(empty)" } else { screen }
    )
}

const fn nonzero(value: u16) -> std::num::NonZeroU16 {
    match std::num::NonZeroU16::new(value) {
        Some(value) => value,
        None => std::num::NonZeroU16::MIN,
    }
}

fn key_bytes(key: &str) -> Result<Vec<u8>, String> {
    let bytes: &[u8] = match key {
        "Enter" => b"\r",
        "Tab" => b"\t",
        "Escape" => b"\x1b",
        "Backspace" => b"\x7f",
        "Delete" => b"\x1b[3~",
        "Up" => b"\x1b[A",
        "Down" => b"\x1b[B",
        "Right" => b"\x1b[C",
        "Left" => b"\x1b[D",
        "Home" => b"\x1b[H",
        "End" => b"\x1b[F",
        "PageUp" => b"\x1b[5~",
        "PageDown" => b"\x1b[6~",
        "Space" => b" ",
        _ => {
            if let Some(letter) = key.strip_prefix("Ctrl+") {
                let mut chars = letter.chars();
                if let (Some(c), None) = (chars.next(), chars.next()) {
                    if c.is_ascii_alphabetic() {
                        return Ok(vec![c.to_ascii_lowercase() as u8 & 0x1f]);
                    }
                }
            }
            return Err(format!("unknown key {key}"));
        }
    };
    Ok(bytes.to_vec())
}

fn cut(text: &str, chars: usize) -> String {
    match text.char_indices().nth(chars) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_log_stops_at_its_limit_with_a_note() {
        let mut log = CappedLog {
            out: Vec::new(),
            written: 0,
            limit: 10,
        };
        log.write(b"123456");
        log.write(b"789012");
        log.write(b"never");
        let text = String::from_utf8(log.out).expect("text");
        assert_eq!(text, format!("123456789012{LOG_CUT}"));
    }

    #[test]
    fn only_plain_reading_commands_count_as_reading() {
        for powershell in [false, true] {
            for command in [
                "ls -la",
                "git status",
                "git log --oneline -5",
                "git diff --stat HEAD~1",
                "git grep -n TODO",
                "rg -n TODO src",
                "rg 'a b' --glob '*.rs'",
                "find . -name '*.rs' -type f",
                "fd -e rs src",
                "head -n 20 README.md",
            ] {
                assert!(reads_only_in(command, powershell), "{command}");
            }
            for command in [
                "mkdir x",
                "git push",
                "git branch -D old",
                "git -c core.pager=sh log",
                "git diff --output=x",
                "git diff --ext-diff",
                "git grep -O TODO",
                "git grep -Ovim TODO",
                "rg --pre ./evil TODO",
                "rg --pre=./evil TODO",
                "rg -z TODO",
                "fd -x rm",
                "fd --exec rm",
                "find . -name x -delete",
                "find . -exec rm {} ;",
                "find . -fprint out",
                "file -C -m x",
                "date 010100002030",
                "cat a > b",
                "ls | xargs rm",
                "ls; rm -rf .",
                "cat $(echo x)",
                "Get-Content (Remove-Item x)",
                "Get-Content @(Remove-Item x)",
                "Get-Content $env:USERPROFILE\\.ssh\\id_rsa",
                "rg 'unclosed",
                "",
            ] {
                assert!(!reads_only_in(command, powershell), "{command}");
            }
        }
        assert!(reads_only_in("Get-ChildItem -Recurse -Filter *.rs", true));
        assert!(reads_only_in("Get-Content src\\main.rs", true));
        assert!(!reads_only_in("Get-ChildItem -Recurse", false));
        assert!(
            !reads_only_in("rg -\\-pre=x TODO", false),
            "an escape makes --pre"
        );
        assert!(
            !reads_only_in("rg --'pre'=x TODO", true),
            "quotes make --pre"
        );
    }

    #[test]
    fn typing_into_a_job_is_a_change_and_waiting_is_not() {
        assert!(job_types(r#"{"id": 1, "input": "!sh\n"}"#));
        assert!(job_types(r#"{"id": 1, "keys": ["Enter"]}"#));
        assert!(!job_types(r#"{"id": 1, "wait_seconds": 5}"#));
        assert!(!job_types(r#"{"id": 1, "stop": true, "input": ""}"#));
        assert!(job_types("not json"));
    }

    #[test]
    fn a_log_is_never_replaced() {
        let dir = tempfile::tempdir().expect("dir");
        std::fs::write(dir.path().join("old.log"), "kept").expect("write");
        let (first, _) = new_log(dir.path(), 1).expect("log");
        let (second, _) = new_log(dir.path(), 1).expect("log");
        assert_ne!(first, second);
        let name = first.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            name.starts_with("job-1-") && name.ends_with(".log"),
            "{name}"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("old.log")).unwrap(),
            "kept"
        );
    }

    #[test]
    fn keys_become_terminal_bytes() {
        assert_eq!(key_bytes("Enter").unwrap(), b"\r");
        assert_eq!(key_bytes("Up").unwrap(), b"\x1b[A");
        assert_eq!(key_bytes("Ctrl+C").unwrap(), vec![3]);
        assert_eq!(key_bytes("Ctrl+c").unwrap(), vec![3]);
        assert_eq!(key_bytes("F13").unwrap_err(), "unknown key F13");
    }

    #[test]
    fn long_output_shows_its_first_and_last_lines() {
        let owned: Vec<String> = (1..=50).map(|n| format!("line {n}")).collect();
        let lines: Vec<&str> = owned.iter().map(String::as_str).collect();
        let shown = edges(&lines);
        assert!(shown.starts_with("\nline 1\n"));
        assert!(shown.contains("line 20\n… 10 lines not shown …\nline 31\n"));
        assert!(shown.ends_with("line 50"));
        assert_eq!(edges(&lines[..3]), "\nline 1\nline 2\nline 3");
    }
}
