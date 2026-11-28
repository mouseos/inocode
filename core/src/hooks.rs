//! Hooks: commands the person sets to run at points of a thread's work, in
//! Claude Code's format, from the settings and from a project's
//! `.inocode/settings.json` once the person trusts it
//! (docs/evidence/hooks.md).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde_json::{json, Map, Value};
use sha2::Digest;
use tokio::io::AsyncWriteExt;

use crate::store::Store;

/// How long a hook may run when it names no `timeout`, as in Claude Code.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// Characters of a hook's output kept in a message.
const SHOWN_CHARS: usize = 2_000;

/// The project's file of hooks, beside its other inocode files.
const PROJECT_FILE: &str = ".inocode/settings.json";

/// The trusted projects' hooks, as `{folder: sha256 of the hooks}`.
const TRUSTED_FILE: &str = "hooks-trusted.json";

/// Inocode's tools under the names Claude Code gives the same tools, so
/// that a matcher written for Claude Code matches them too.
const CLAUDE_NAMES: &[(&str, &str)] = &[
    ("shell", "Bash"),
    ("job", "Bash"),
    ("read", "Read"),
    ("write", "Write"),
    ("edit", "Edit"),
    ("edits", "MultiEdit"),
    ("find", "Grep"),
    ("fetch", "WebFetch"),
    ("search", "WebSearch"),
    ("plan", "TodoWrite"),
    ("explore", "Task"),
    ("delegate", "Task"),
];

struct Hook {
    event: String,
    /// The `|`-separated names the matcher takes; empty takes every one.
    matcher: Vec<String>,
    command: String,
    timeout: Duration,
}

/// The hooks a thread's work runs.
#[derive(Default)]
pub(crate) struct Hooks {
    hooks: Vec<Hook>,
    folder: Option<PathBuf>,
    session: i64,
    /// Why some hooks are not used, for the turn's warnings.
    pub warnings: Vec<String>,
}

/// What the hooks of one point said, put together.
#[derive(Debug, Default)]
pub(crate) struct Said {
    /// Why the action is blocked (exit code 2 or `decision: "block"`).
    pub block: Option<String>,
    /// `continue: false`: why the work stops after this point.
    pub stop: Option<String>,
    /// Text for the model (`additionalContext`, or plain output where that
    /// is the meaning).
    pub context: Vec<String>,
    /// `allow`, `deny` or `ask` with its reason: the strictest one.
    pub permission: Option<(String, Option<String>)>,
    pub updated_input: Option<Value>,
    /// For the person: `systemMessage`, and hooks that failed.
    pub messages: Vec<String>,
}

fn parse_hooks(value: &Value, warnings: &mut Vec<String>, place: &str) -> Vec<Hook> {
    let mut hooks = Vec::new();
    let Some(events) = value.as_object() else {
        if !value.is_null() {
            warnings.push(format!(
                "the hooks in {place} are not an object and are not used"
            ));
        }
        return hooks;
    };
    for (event, groups) in events {
        for group in groups.as_array().into_iter().flatten() {
            let matcher = group
                .get("matcher")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let matcher: Vec<String> = match matcher.trim() {
                "" | "*" => Vec::new(),
                names => names
                    .split('|')
                    .map(|name| name.trim().to_string())
                    .collect(),
            };
            for hook in group
                .get("hooks")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                match (hook.get("type").and_then(Value::as_str), hook.get("command").and_then(Value::as_str)) {
                    (Some("command"), Some(command)) if !command.trim().is_empty() => {
                        let timeout = hook
                            .get("timeout")
                            .and_then(Value::as_f64)
                            .filter(|seconds| *seconds > 0.0)
                            .map_or(DEFAULT_TIMEOUT, Duration::from_secs_f64);
                        hooks.push(Hook {
                            event: event.clone(),
                            matcher: matcher.clone(),
                            command: command.to_string(),
                            timeout,
                        });
                    }
                    (kind, _) => warnings.push(format!(
                        "a {event} hook in {place} of type {} is not used: only \"command\" hooks run here",
                        kind.unwrap_or("(none)")
                    )),
                }
            }
        }
    }
    hooks
}

fn digest(hooks: &Value) -> String {
    let text = serde_json::to_string(hooks).unwrap_or_default();
    sha2::Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn trusted_path(store: &Store) -> PathBuf {
    store.files_folder().with_file_name(TRUSTED_FILE)
}

fn read_trusted(store: &Store) -> Map<String, Value> {
    std::fs::read_to_string(trusted_path(store))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// The hooks of a project folder's file, `Null` when it has none.
fn project_hooks(folder: &Path) -> Result<Value, String> {
    let path = folder.join(PROJECT_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Value::Null),
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    let file: Value =
        serde_json::from_str(&text).map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(file.get("hooks").cloned().unwrap_or(Value::Null))
}

fn folder_key(folder: &Path) -> String {
    folder.to_string_lossy().into_owned()
}

impl Hooks {
    /// The hooks of the settings beside `store`, and those of `folder`'s
    /// project when the person trusted them as they now are.
    pub(crate) fn load(store: &Store, session: i64, folder: Option<&Path>) -> Self {
        let mut warnings = Vec::new();
        let settings = store.files_folder().with_file_name("settings.json");
        let global = std::fs::read_to_string(&settings)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .and_then(|file| file.get("hooks").cloned())
            .unwrap_or(Value::Null);
        let mut hooks = parse_hooks(&global, &mut warnings, "the settings");
        if let Some(folder) = folder {
            match project_hooks(folder) {
                Ok(Value::Null) => {}
                Ok(project) => {
                    let trusted = read_trusted(store)
                        .get(&folder_key(folder))
                        .and_then(Value::as_str)
                        .is_some_and(|hash| hash == digest(&project));
                    match trusted {
                        true => hooks.extend(parse_hooks(&project, &mut warnings, PROJECT_FILE)),
                        false => warnings.push(format!(
                            "the project's hooks in {PROJECT_FILE} are not used until the person \
                             trusts them as they are now (hooks/trust)"
                        )),
                    }
                }
                Err(error) => warnings.push(format!("the project's hooks are not used: {error}")),
            }
        }
        Self {
            hooks,
            folder: folder.map(Path::to_path_buf),
            session,
            warnings,
        }
    }

    pub(crate) fn has(&self, event: &str) -> bool {
        self.hooks.iter().any(|hook| hook.event == event)
    }

    /// Runs the hooks of `event` whose matcher takes `target` (the tool's
    /// name, or the kind of the point), one after another, with `input`
    /// and the fields every hook gets.
    pub(crate) async fn run(&self, event: &str, target: Option<&str>, input: Value) -> Said {
        let mut said = Said::default();
        let alias = target.and_then(|target| {
            CLAUDE_NAMES
                .iter()
                .find(|(name, _)| *name == target)
                .map(|(_, alias)| *alias)
        });
        let chosen = self.hooks.iter().filter(|hook| {
            hook.event == event
                && (hook.matcher.is_empty()
                    || hook
                        .matcher
                        .iter()
                        .any(|name| Some(name.as_str()) == target || Some(name.as_str()) == alias))
        });
        let mut input = input;
        if let Some(fields) = input.as_object_mut() {
            fields.insert("session_id".into(), json!(self.session.to_string()));
            fields.insert("hook_event_name".into(), json!(event));
            fields.insert(
                "cwd".into(),
                json!(self.folder.as_ref().map(|folder| folder_key(folder))),
            );
        }
        for hook in chosen {
            run_one(hook, self.folder.as_deref(), &input, &mut said).await;
        }
        said
    }
}

fn cut(text: &str) -> String {
    let text = text.trim();
    let mut kept: String = text.chars().take(SHOWN_CHARS).collect();
    if kept.len() < text.len() {
        kept.push('…');
    }
    kept
}

async fn run_one(hook: &Hook, folder: Option<&Path>, input: &Value, said: &mut Said) {
    let argv = crate::shell::shell_command(&hook.command);
    let mut command = tokio::process::Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(folder) = folder {
        command
            .current_dir(folder)
            .env("CLAUDE_PROJECT_DIR", folder)
            .env("INOCODE_PROJECT_DIR", folder);
    }
    let name = &hook.event;
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            said.messages
                .push(format!("a {name} hook could not start: {error}"));
            return;
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(input.to_string().as_bytes()).await;
    }
    let output = match tokio::time::timeout(hook.timeout, child.wait_with_output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => {
            said.messages.push(format!("a {name} hook failed: {error}"));
            return;
        }
        Err(_) => {
            said.messages.push(format!(
                "a {name} hook was stopped after {}s",
                hook.timeout.as_secs_f64()
            ));
            return;
        }
    };
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    match output.status.code() {
        Some(0) => read_output(name, stdout.trim(), said),
        Some(2) => {
            said.block.get_or_insert_with(|| match cut(&stderr) {
                why if why.is_empty() => format!("a {name} hook blocked it"),
                why => why,
            });
        }
        code => said.messages.push(format!(
            "a {name} hook failed (exit code {}): {}",
            code.map_or_else(|| "none".to_string(), |code| code.to_string()),
            cut(&stderr)
        )),
    }
}

/// Reads what a hook that ended well printed: JSON in Claude Code's shape,
/// or plain text, which is context for the model at the points where
/// Claude Code takes it so.
fn read_output(event: &str, stdout: &str, said: &mut Said) {
    if stdout.is_empty() {
        return;
    }
    let Ok(Value::Object(output)) = serde_json::from_str::<Value>(stdout) else {
        if matches!(event, "UserPromptSubmit" | "SessionStart") {
            said.context.push(cut(stdout));
        }
        return;
    };
    let text = |value: Option<&Value>| value.and_then(Value::as_str).map(cut);
    if output.get("continue").and_then(Value::as_bool) == Some(false) {
        said.stop.get_or_insert_with(|| {
            text(output.get("stopReason"))
                .unwrap_or_else(|| format!("a {event} hook stopped the work"))
        });
    }
    if let Some(message) = text(output.get("systemMessage")) {
        said.messages.push(message);
    }
    if output.get("decision").and_then(Value::as_str) == Some("block") {
        said.block.get_or_insert_with(|| {
            text(output.get("reason")).unwrap_or_else(|| format!("a {event} hook blocked it"))
        });
    }
    let specific = output.get("hookSpecificOutput");
    let field = |name: &str| specific.and_then(|specific| specific.get(name));
    if let Some(context) = text(field("additionalContext")) {
        said.context.push(context);
    }
    let decision = field("permissionDecision")
        .and_then(Value::as_str)
        .map(|decision| {
            (
                decision.to_string(),
                text(field("permissionDecisionReason")),
            )
        });
    // PermissionRequest's answer: `{behavior: allow | deny, message}`.
    let decision = decision.or_else(|| {
        let answer = field("decision")?;
        let behavior = answer.get("behavior").and_then(Value::as_str)?;
        Some((behavior.to_string(), text(answer.get("message"))))
    });
    if let Some((decision, reason)) =
        decision.filter(|(decision, _)| matches!(decision.as_str(), "allow" | "deny" | "ask"))
    {
        let rank = |decision: &str| match decision {
            "deny" => 3,
            "ask" => 2,
            _ => 1,
        };
        if said
            .permission
            .as_ref()
            .is_none_or(|(was, _)| rank(&decision) > rank(was))
        {
            said.permission = Some((decision, reason));
        }
    }
    let updated = field("updatedInput")
        .or_else(|| field("decision").and_then(|answer| answer.get("updatedInput")));
    if let Some(updated) = updated.filter(|updated| updated.is_object()) {
        said.updated_input = Some(updated.clone());
    }
}

/// What `hooks/list` shows for a thread's folder: the settings' hooks and
/// the project's, with whether the project's are trusted as they are.
pub(crate) fn listing(store: &Store, folder: Option<&Path>) -> Value {
    let settings = store.files_folder().with_file_name("settings.json");
    let global = std::fs::read_to_string(settings)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|file| file.get("hooks").cloned())
        .unwrap_or(Value::Null);
    let project = folder.map(|folder| match project_hooks(folder) {
        Ok(hooks) => {
            let trusted = !hooks.is_null()
                && read_trusted(store)
                    .get(&folder_key(folder))
                    .and_then(Value::as_str)
                    .is_some_and(|hash| hash == digest(&hooks));
            json!({
                "path": folder.join(PROJECT_FILE).to_string_lossy(),
                "hooks": hooks,
                "trusted": trusted,
            })
        }
        Err(error) => {
            json!({ "path": folder.join(PROJECT_FILE).to_string_lossy(), "error": error })
        }
    });
    json!({ "settings": global, "project": project })
}

/// Trusts `folder`'s project hooks as they are now; a later change to them
/// needs trusting again. `false` when the project has no hooks.
pub(crate) fn trust(store: &Store, folder: &Path) -> Result<bool, String> {
    let hooks = project_hooks(folder)?;
    if hooks.is_null() {
        return Ok(false);
    }
    let mut trusted = read_trusted(store);
    trusted.insert(folder_key(folder), json!(digest(&hooks)));
    let path = trusted_path(store);
    let partial = path.with_extension("json.partial");
    let text = serde_json::to_string_pretty(&trusted).map_err(|error| error.to_string())?;
    std::fs::write(&partial, text)
        .and_then(|()| std::fs::rename(&partial, &path))
        .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    Ok(true)
}
