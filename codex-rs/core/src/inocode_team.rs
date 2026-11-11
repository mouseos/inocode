//! Keeps the `codex-inocode-team` roster in step with the agents' real statuses and token use.

use codex_protocol::ThreadId;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::TokenUsage;

fn state_of(status: &AgentStatus) -> codex_inocode_team::AgentState {
    use codex_inocode_team::AgentState;
    match status {
        AgentStatus::PendingInit | AgentStatus::Running => AgentState::Running,
        AgentStatus::Interrupted => AgentState::Waiting,
        AgentStatus::Errored(_) => AgentState::Blocked,
        _ => AgentState::Done,
    }
}

/// Updates the state of each known child of `root` from its current status.
pub(crate) fn sync_children<'a>(
    root: ThreadId,
    children: impl Iterator<Item = (ThreadId, &'a AgentStatus)>,
) {
    let roster = codex_inocode_team::roster_for(&root.to_string());
    for (id, status) in children {
        roster.set_state(&id.to_string(), state_of(status));
    }
}

/// Adds one response's token use to the thread's total.
pub(crate) fn record_tokens(thread_id: ThreadId, usage: Option<&TokenUsage>) {
    let Some(usage) = usage else { return };
    let total = u64::try_from(usage.input_tokens.max(0) + usage.output_tokens.max(0)).unwrap_or(0);
    codex_inocode_team::add_thread_tokens(&thread_id.to_string(), total);
}

/// Follows one child's status channel and mirrors every change into the root's roster, so the
/// roster does not depend on `wait_agent` being called.
pub(crate) fn watch_child(session: &std::sync::Arc<crate::session::session::Session>, child: ThreadId) {
    use futures::StreamExt;
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let control = session
        .services
        .local_agent_runtime
        .control(session.session_id());
    let roster = codex_inocode_team::roster_for(&session.thread_id.to_string());
    let child_id = child.to_string();
    handle.spawn(async move {
        let Ok(mut updates) = control.subscribe_status(child).await else {
            return;
        };
        while let Some(update) = updates.next().await {
            let Ok(info) = update else { continue };
            if let Some(status) = info.status() {
                roster.set_state(&child_id, state_of(status));
            }
        }
    });
}
