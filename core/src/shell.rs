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
use serde::Deserialize;
use serde_json::json;
use tokio::sync::watch;

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

/// Whether `command` only reads: one plain command (no chaining, pipes,
/// redirections, substitutions or variables) whose program and first
/// argument are on a list of reading commands, without a flag that deletes
/// or runs something.
pub(crate) fn reads_only(command: &str) -> bool {
    const READERS: &[&str] = &[
        "ls",
        "dir",
        "pwd",
        "cat",
        "type",
        "head",
        "tail",
        "wc",
        "grep",
        "rg",
        "find",
        "fd",
        "tree",
        "stat",
        "file",
        "which",
        "where",
        "du",
        "df",
        "date",
        "whoami",
        "get-childitem",
        "get-content",
        "get-location",
        "get-item",
        "select-string",
        "test-path",
        "resolve-path",
        "get-command",
    ];
    const GIT_READERS: &[&str] = &[
        "status",
        "diff",
        "log",
        "show",
        "blame",
        "rev-parse",
        "ls-files",
        "grep",
    ];
    const UNSAFE: &[&str] = &[
        "-delete", "-exec", "-execdir", "-ok", "-fprint", "-fls", "-fprintf",
    ];
    let command = command.trim();
    if command.is_empty()
        || command
            .chars()
            .any(|c| matches!(c, ';' | '&' | '|' | '>' | '<' | '`' | '$' | '\n' | '\r'))
    {
        return false;
    }
    let words: Vec<String> = command
        .split_whitespace()
        .map(str::to_ascii_lowercase)
        .collect();
    if words.iter().any(|word| UNSAFE.contains(&word.as_str())) {
        return false;
    }
    match words[0].as_str() {
        "git" => words
            .get(1)
            .is_some_and(|sub| GIT_READERS.contains(&sub.as_str())),
        program => READERS.contains(&program),
    }
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

fn start(key: Key, folder: &Path, command: String, confined: bool) -> Result<Arc<Job>, String> {
    let id = {
        let mut registry = registry().lock().map_err(|_| "the job list is unusable")?;
        let jobs = registry.entry(key.clone()).or_default();
        jobs.next += 1;
        jobs.next
    };
    let logs = key.0.join(format!("thread-{}", key.1 .0));
    std::fs::create_dir_all(&logs)
        .map_err(|error| format!("could not create {}: {error}", logs.display()))?;
    let log = logs.join(format!("job-{id}.log"));
    let file = File::create(&log)
        .map_err(|error| format!("could not create {}: {error}", log.display()))?;

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
    });
    registry()
        .lock()
        .map_err(|_| "the job list is unusable")?
        .entry(key)
        .or_default()
        .jobs
        .push(Arc::clone(&job));

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
    });
    Ok(job)
}

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
        for command in [
            "ls -la",
            "git status",
            "git log --oneline -5",
            "Get-Content README.md",
            "rg TODO src",
            "find . -name '*.rs'",
        ] {
            assert!(reads_only(command), "{command}");
        }
        for command in [
            "mkdir x",
            "git push",
            "git branch -D old",
            "find . -name x -delete",
            "find . -exec rm {} ;",
            "cat a > b",
            "ls | xargs rm",
            "ls; rm -rf .",
            "cat $(echo x)",
            "Get-Content $env:USERPROFILE\\.ssh\\id_rsa",
            "",
        ] {
            assert!(!reads_only(command), "{command}");
        }
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
