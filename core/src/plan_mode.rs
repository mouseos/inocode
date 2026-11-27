//! Plan mode: the model investigates without changing files and presents
//! a plan with `exit_plan`, which the person approves or sends back with
//! feedback (docs/evidence/plan-mode.md).

use serde_json::{json, Value};

use crate::conversation::ToolSpec;
use crate::store::{SessionId, Store};
use crate::ErrorInfo;

pub(crate) const EXIT_PLAN: &str = "exit_plan";

/// The `mode` of the elicitation that shows the plan to the person.
pub(crate) const PLAN_MODE: &str = "plan";

/// The field of the thread's stored settings that turns plan mode on.
const FIELD: &str = "plan";

/// Sent with every request while plan mode is on.
pub(crate) const STATE: &str = "Plan mode is on: the person wants a plan before any change. \
Investigate with actions that only read (read, find, explore, recall, search, fetch, commands \
that only read) and do not change files: edit, edits, write, plugin installs, delegation, \
photos and recordings are refused, and other commands, input to running commands and MCP \
tools wait for the person's approval. When the plan is ready, present it with exit_plan.";

/// The result of a call refused in plan mode.
pub(crate) const REFUSED: &str = "Not run: plan mode is on, so nothing is changed yet. \
Investigate with actions that only read, then present your plan with exit_plan.";

/// Why a call waits for the person in plan mode.
pub(crate) const ASKS: &str = "plan mode is on, and this may change something";

pub(crate) fn spec() -> ToolSpec {
    ToolSpec {
        name: EXIT_PLAN.into(),
        description: "Presents your plan to the person for approval; offered only in plan \
            mode. Write it in markdown: what you found, the changes you will make, file by \
            file, and how you will check them. If the person approves, plan mode ends and \
            you implement the plan at once in the same turn; if not, you get their feedback, \
            revise the plan, and present it again."
            .into(),
        parameters: json!({
            "type": "object",
            "required": ["plan"],
            "properties": {
                "plan": {"type": "string", "description": "The plan, in markdown."}
            }
        }),
    }
}

/// The plan of an `exit_plan` call.
pub(crate) fn plan(arguments: &str) -> Result<String, String> {
    serde_json::from_str::<Value>(arguments)
        .ok()
        .and_then(|value| value.get("plan")?.as_str().map(str::to_string))
        .filter(|plan| !plan.trim().is_empty())
        .ok_or_else(|| "exit_plan needs a non-empty \"plan\" string".to_string())
}

/// Whether plan mode is on for `session`, as its stored settings say.
pub(crate) async fn on(store: &Store, session: SessionId) -> Result<bool, ErrorInfo> {
    let Some(text) = store.thread_settings(session).await? else {
        return Ok(false);
    };
    let settings: Value = serde_json::from_str(&text)
        .map_err(|error| ErrorInfo::new("store", format!("thread settings: {error}")))?;
    Ok(settings.get(FIELD).and_then(Value::as_bool) == Some(true))
}

/// Turns plan mode on or off for `session`, keeping its other settings.
pub(crate) async fn set(store: &Store, session: SessionId, on: bool) -> Result<(), ErrorInfo> {
    let mut settings = match store.thread_settings(session).await? {
        Some(text) => serde_json::from_str(&text)
            .map_err(|error| ErrorInfo::new("store", format!("thread settings: {error}")))?,
        None => json!({}),
    };
    let fields = settings
        .as_object_mut()
        .ok_or_else(|| ErrorInfo::new("store", "thread settings: not an object"))?;
    match on {
        true => fields.insert(FIELD.into(), json!(true)),
        false => fields.remove(FIELD),
    };
    store
        .set_thread_settings(session, settings.to_string())
        .await
}
