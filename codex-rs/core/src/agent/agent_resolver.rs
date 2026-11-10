use crate::function_tool::FunctionCallError;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use codex_inocode_flags::InocodeFlags;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErrorDetails;
use std::sync::Arc;

/// Resolves a single tool-facing agent target to a thread id.
///
/// With `peer_messaging.enabled`, a target that does not resolve under the caller may name a
/// sibling (same parent): either `../name` or a bare `name` that exists next to the caller.
pub(crate) async fn resolve_agent_target(
    session: &Arc<Session>,
    turn: &Arc<TurnContext>,
    target: &str,
) -> Result<ThreadId, FunctionCallError> {
    let resolved = session
        .services
        .agent_control
        .resolve(
            session.thread_id,
            turn.parent_thread_id,
            &turn.session_source,
            target,
        )
        .await;
    let err = match resolved {
        Ok(thread_id) => return Ok(thread_id),
        Err(err) => err,
    };
    if InocodeFlags::load().peer_messaging_enabled
        && let Some(my_path) = turn.session_source.get_agent_path()
        && let Some(sibling) = sibling_reference(my_path.as_str(), target)
        && let Ok(thread_id) = session
            .services
            .agent_control
            .resolve(
                session.thread_id,
                turn.parent_thread_id,
                &turn.session_source,
                &sibling,
            )
            .await
    {
        return Ok(thread_id);
    }
    Err(match err.details() {
        CodexErrorDetails::UnsupportedOperation(message) => {
            FunctionCallError::RespondToModel(message.clone())
        }
        _ => FunctionCallError::RespondToModel(err.to_string()),
    })
}

/// Absolute path of the sibling named by `target` (`../name` or `name`) for the agent at
/// `my_path`, or `None` when `target` cannot name a sibling.
pub(crate) fn sibling_reference(my_path: &str, target: &str) -> Option<String> {
    let name = target.strip_prefix("../").unwrap_or(target);
    if name.is_empty() || name.contains('/') || name == ".." {
        return None;
    }
    let (parent, _) = my_path.rsplit_once('/')?;
    // The root agent (`/root`) has no siblings.
    if parent.is_empty() {
        return None;
    }
    Some(format!("{parent}/{name}"))
}

#[cfg(test)]
mod tests {
    use super::sibling_reference;
    use pretty_assertions::assert_eq;

    #[test]
    fn sibling_names_resolve_next_to_the_caller() {
        assert_eq!(
            sibling_reference("/root/a", "b").as_deref(),
            Some("/root/b")
        );
        assert_eq!(
            sibling_reference("/root/a", "../b").as_deref(),
            Some("/root/b")
        );
        assert_eq!(
            sibling_reference("/root/x/a", "b").as_deref(),
            Some("/root/x/b")
        );
        assert_eq!(sibling_reference("/root", "b"), None);
        assert_eq!(sibling_reference("/root/a", "b/c"), None);
        assert_eq!(sibling_reference("/root/a", "/root/b"), None);
        assert_eq!(sibling_reference("/root/a", ".."), None);
    }
}
