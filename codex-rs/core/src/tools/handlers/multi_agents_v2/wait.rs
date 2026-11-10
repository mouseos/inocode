use super::*;
use crate::agent::status::is_final;
use crate::session::InputQueueActivity;
use crate::thread_input::InputWaitOutcome as WaitOutcome;
use crate::thread_input::subscribe_input_activity;
use crate::thread_input::wait_for_activity;
use crate::tools::handlers::multi_agents_spec::WaitAgentTimeoutOptions;
use crate::tools::handlers::multi_agents_spec::create_wait_agent_tool_v2;
use codex_inocode_flags::InocodeFlags;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_tools::ToolSpec;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::Instant;

/// How often child statuses are re-read while waiting, as insurance against a missed
/// completion notification.
const STATUS_RECHECK_INTERVAL: Duration = Duration::from_secs(5);

/// Children whose final result was already returned by `wait_agent`, per waiting thread.
fn reported_children() -> &'static Mutex<HashMap<ThreadId, HashSet<ThreadId>>> {
    static REPORTED: OnceLock<Mutex<HashMap<ThreadId, HashSet<ThreadId>>>> = OnceLock::new();
    REPORTED.get_or_init(Default::default)
}

#[derive(Default)]
pub(crate) struct Handler {
    options: WaitAgentTimeoutOptions,
}

impl Handler {
    pub(crate) fn new(options: WaitAgentTimeoutOptions) -> Self {
        Self { options }
    }
}

impl ToolExecutor<ToolInvocation> for Handler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("wait_agent")
    }

    fn spec(&self) -> ToolSpec {
        create_wait_agent_tool_v2(self.options)
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(self.handle_call(invocation))
    }
}

impl Handler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let ToolInvocation {
            session,
            turn,
            payload,
            call_id,
            ..
        } = invocation;
        let arguments = function_arguments(payload)?;
        let args: WaitArgs = parse_arguments(&arguments)?;
        let min_timeout_ms = turn.config.multi_agent_v2.min_wait_timeout_ms;
        let max_timeout_ms = turn.config.multi_agent_v2.max_wait_timeout_ms;
        let default_timeout_ms = turn.config.multi_agent_v2.default_wait_timeout_ms;
        let requested_timeout_ms = args.timeout_ms;
        let timeout_ms = match requested_timeout_ms {
            Some(ms) if ms > max_timeout_ms => {
                return Err(FunctionCallError::RespondToModel(format!(
                    "timeout_ms must be at most {max_timeout_ms}"
                )));
            }
            Some(ms) => ms.max(min_timeout_ms),
            None => default_timeout_ms,
        };

        let (activity_rx, pending_activity) =
            subscribe_input_activity(&session, &turn.sub_id).await;

        session
            .emit_turn_item_started(
                &turn,
                &TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
                    id: call_id.clone(),
                    tool: CollabAgentTool::Wait,
                    status: CollabAgentToolCallStatus::InProgress,
                    sender_thread_id: session.thread_id,
                    receiver_thread_ids: Vec::new(),
                    receiver_agents: Vec::new(),
                    prompt: None,
                    model: None,
                    reasoning_effort: None,
                    agents_states: Default::default(),
                }),
            )
            .await;

        crate::inocode_loop::record_wait_agent_call(session.thread_id);
        let wait_started = Instant::now();
        let event_driven = InocodeFlags::load().wait_event_driven;
        let (outcome, completed, pending) = if event_driven {
            let my_path = turn
                .session_source
                .get_agent_path()
                .unwrap_or_else(AgentPath::root);
            wait_for_children(
                &session,
                &turn,
                my_path.as_str(),
                args.mode.unwrap_or_default(),
                Duration::from_millis(timeout_ms as u64),
                activity_rx,
                pending_activity,
            )
            .await
        } else {
            let outcome = tokio::time::timeout(
                Duration::from_millis(timeout_ms as u64),
                wait_for_activity(activity_rx, pending_activity),
            )
            .await
            .unwrap_or(WaitOutcome::TimedOut);
            (outcome, Vec::new(), Vec::new())
        };
        // A completed wait may wake for a message, user input, or its timeout.
        // Dropped waits do not have an observed outcome and are not included.
        turn.session_telemetry.record_duration(
            "codex.multi_agent.wait.duration_ms",
            wait_started.elapsed(),
            &[(
                "outcome",
                match outcome {
                    WaitOutcome::MailboxActivity => "mailbox",
                    WaitOutcome::Steered => "steered",
                    WaitOutcome::TimedOut => "timed_out",
                },
            )],
        );
        let mut result = WaitAgentResult::from_outcome(outcome, requested_timeout_ms, timeout_ms);
        result.completed = completed;
        result.pending = pending;

        session
            .emit_turn_item_completed(
                &turn,
                TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
                    id: call_id,
                    tool: CollabAgentTool::Wait,
                    status: CollabAgentToolCallStatus::Completed,
                    sender_thread_id: session.thread_id,
                    receiver_thread_ids: Vec::new(),
                    receiver_agents: Vec::new(),
                    prompt: None,
                    model: None,
                    reasoning_effort: None,
                    agents_states: HashMap::new(),
                }),
            )
            .await;

        Ok(boxed_tool_output(result))
    }
}

impl CoreToolRuntime for Handler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitArgs {
    timeout_ms: Option<i64>,
    mode: Option<WaitMode>,
}

/// Which child completions end the wait.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WaitMode {
    /// Return as soon as one child finishes (or any mailbox message arrives).
    #[default]
    Any,
    /// Return once every running child has finished.
    All,
}

/// Final result of one child agent.
#[derive(Debug, Deserialize, Serialize, PartialEq, Eq, Clone)]
pub(crate) struct CompletedAgent {
    pub(crate) agent: String,
    pub(crate) status: String,
    /// The child's last output text (or error message).
    pub(crate) output: Option<String>,
}

#[derive(Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct WaitAgentResult {
    pub(crate) message: String,
    pub(crate) timed_out: bool,
    /// Children that finished since the last wait, with their final output.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) completed: Vec<CompletedAgent>,
    /// Direct children that are still running.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) pending: Vec<String>,
}

impl WaitAgentResult {
    fn from_outcome(
        outcome: WaitOutcome,
        requested_timeout_ms: Option<i64>,
        timeout_ms: i64,
    ) -> Self {
        let message = match outcome {
            WaitOutcome::MailboxActivity => "Wait completed.",
            WaitOutcome::Steered => "Wait interrupted by new input.",
            WaitOutcome::TimedOut => "Wait timed out.",
        };
        let message = match requested_timeout_ms {
            Some(requested_timeout_ms) if requested_timeout_ms < timeout_ms => format!(
                "{message}\n\nRequested timeout of {requested_timeout_ms}ms was clamped to the minimum of {timeout_ms}ms."
            ),
            Some(_) | None => message.to_string(),
        };
        Self {
            message,
            timed_out: outcome == WaitOutcome::TimedOut,
            completed: Vec::new(),
            pending: Vec::new(),
        }
    }
}

impl ToolOutput for WaitAgentResult {
    fn log_output(&self) -> String {
        tool_output_json_text(self, "wait_agent")
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        tool_output_response_item(call_id, payload, self, /*success*/ None, "wait_agent")
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        tool_output_code_mode_result(self, "wait_agent")
    }
}

struct ChildSnapshot {
    thread_id: ThreadId,
    name: String,
    status: AgentStatus,
}

fn status_label(status: &AgentStatus) -> &'static str {
    match status {
        AgentStatus::PendingInit => "pending_init",
        AgentStatus::Running => "running",
        AgentStatus::Interrupted => "interrupted",
        AgentStatus::Completed(_) => "completed",
        AgentStatus::Errored(_) => "errored",
        AgentStatus::Shutdown => "shutdown",
        AgentStatus::NotFound => "not_found",
    }
}

fn status_output(status: &AgentStatus) -> Option<String> {
    match status {
        AgentStatus::Completed(text) => text.clone(),
        AgentStatus::Errored(message) => Some(message.clone()),
        _ => None,
    }
}

async fn direct_children(
    session: &Arc<crate::session::session::Session>,
    turn: &Arc<crate::session::turn_context::TurnContext>,
    my_path: &str,
) -> Vec<ChildSnapshot> {
    let Ok(agents) = session
        .services
        .agent_control
        .list(
            session.thread_id,
            turn.parent_thread_id,
            &turn.session_source,
            /*path_prefix*/ None,
        )
        .await
    else {
        return Vec::new();
    };
    agents
        .into_iter()
        .filter_map(|agent| {
            let path = agent.metadata.agent_path.as_ref()?.as_str().to_string();
            let (parent, _) = path.rsplit_once('/')?;
            (parent == my_path && agent.thread_id != session.thread_id).then_some(ChildSnapshot {
                thread_id: agent.thread_id,
                name: path,
                status: agent.status,
            })
        })
        .collect()
}

type WaitResult = (WaitOutcome, Vec<CompletedAgent>, Vec<String>);

fn plain(outcome: WaitOutcome) -> WaitResult {
    (outcome, Vec::new(), Vec::new())
}

/// Waits until the requested children finish, returning their results.
///
/// Wakes early for steered user input and (in `Any` mode) for any mailbox message, so queued
/// questions and blocks are not hidden. The timeout is the insurance for lost notifications,
/// and statuses are re-read periodically for the same reason.
async fn wait_for_children(
    session: &Arc<crate::session::session::Session>,
    turn: &Arc<crate::session::turn_context::TurnContext>,
    my_path: &str,
    mode: WaitMode,
    timeout: Duration,
    mut activity: watch::Receiver<InputQueueActivity>,
    pending_activity: Option<InputQueueActivity>,
) -> WaitResult {
    let deadline = Instant::now() + timeout;
    let mut mailbox_woke = false;
    match pending_activity {
        Some(InputQueueActivity::Steer) => return plain(WaitOutcome::Steered),
        Some(InputQueueActivity::Mailbox) => mailbox_woke = true,
        None => {}
    }
    loop {
        let reported = reported_children()
            .lock()
            .map(|map| map.get(&session.thread_id).cloned().unwrap_or_default())
            .unwrap_or_default();
        let children = direct_children(session, turn, my_path).await;
        crate::inocode_team::sync_children(
            session.thread_id,
            children.iter().map(|child| (child.thread_id, &child.status)),
        );
        let (finished, running) = split_children(children, &reported);
        let nothing_running = running.is_empty();
        if should_return(mode, !finished.is_empty(), nothing_running, mailbox_woke) {
            let completed = completed_agents(&finished);
            if let Ok(mut map) = reported_children().lock() {
                map.entry(session.thread_id)
                    .or_default()
                    .extend(finished.iter().map(|child| child.thread_id));
            }
            let pending = running.into_iter().map(|child| child.name).collect();
            return (WaitOutcome::MailboxActivity, completed, pending);
        }
        mailbox_woke = false;
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            let pending = running.into_iter().map(|child| child.name).collect();
            return (WaitOutcome::TimedOut, Vec::new(), pending);
        }
        // With no children to watch, a plain activity wait is all that is left.
        let tick = if nothing_running {
            remaining
        } else {
            remaining.min(STATUS_RECHECK_INTERVAL)
        };
        match tokio::time::timeout(tick, activity.changed()).await {
            Ok(Ok(())) => match *activity.borrow_and_update() {
                InputQueueActivity::Steer => return plain(WaitOutcome::Steered),
                InputQueueActivity::Mailbox => mailbox_woke = true,
            },
            Ok(Err(_)) => return plain(WaitOutcome::TimedOut),
            Err(_) => {}
        }
    }
}

/// Splits not-yet-reported children into (finished, still running).
fn split_children(
    children: Vec<ChildSnapshot>,
    reported: &HashSet<ThreadId>,
) -> (Vec<ChildSnapshot>, Vec<ChildSnapshot>) {
    children
        .into_iter()
        .filter(|child| !reported.contains(&child.thread_id))
        .partition(|child| is_final(&child.status))
}

fn completed_agents(finished: &[ChildSnapshot]) -> Vec<CompletedAgent> {
    finished
        .iter()
        .map(|child| CompletedAgent {
            agent: child.name.clone(),
            status: status_label(&child.status).to_string(),
            output: status_output(&child.status),
        })
        .collect()
}

/// Whether the wait ends now: the requested completions happened, or a mailbox message woke
/// an `any` wait (or nothing is left running to wait for).
fn should_return(mode: WaitMode, any_finished: bool, nothing_running: bool, mailbox_woke: bool) -> bool {
    let satisfied = match mode {
        WaitMode::Any => any_finished,
        WaitMode::All => any_finished && nothing_running,
    };
    satisfied || (mailbox_woke && (mode == WaitMode::Any || nothing_running))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn child(name: &str, status: AgentStatus) -> ChildSnapshot {
        ChildSnapshot {
            thread_id: ThreadId::new(),
            name: name.to_string(),
            status,
        }
    }

    #[test]
    fn finished_children_carry_their_final_output_and_are_reported_once() {
        let done = child("/root/a", AgentStatus::Completed(Some("result text".into())));
        let failed = child("/root/c", AgentStatus::Errored("boom".into()));
        let running = child("/root/b", AgentStatus::Running);
        let done_id = done.thread_id;
        let (finished, still) = split_children(vec![done, failed, running], &HashSet::new());
        assert_eq!(
            completed_agents(&finished),
            vec![
                CompletedAgent {
                    agent: "/root/a".into(),
                    status: "completed".into(),
                    output: Some("result text".into()),
                },
                CompletedAgent {
                    agent: "/root/c".into(),
                    status: "errored".into(),
                    output: Some("boom".into()),
                },
            ]
        );
        assert_eq!(still.len(), 1);
        // Once reported, the same child is not returned again.
        let reported: HashSet<ThreadId> = [done_id].into_iter().collect();
        let again = vec![ChildSnapshot {
            thread_id: done_id,
            name: "/root/a".into(),
            status: AgentStatus::Completed(None),
        }];
        assert!(split_children(again, &reported).0.is_empty());
    }

    #[test]
    fn any_returns_on_first_completion_and_all_waits_for_the_rest() {
        assert!(should_return(WaitMode::Any, true, false, false));
        assert!(!should_return(WaitMode::All, true, false, false));
        assert!(should_return(WaitMode::All, true, true, false));
        assert!(!should_return(WaitMode::Any, false, false, false));
    }

    #[test]
    fn mail_wakes_any_but_not_all_while_children_run() {
        assert!(should_return(WaitMode::Any, false, false, true));
        assert!(!should_return(WaitMode::All, false, false, true));
        assert!(should_return(WaitMode::All, false, true, true));
    }
}
