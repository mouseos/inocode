//! Glue between the sampling loop and the account pool (`codex_login::account_pool`).
//!
//! Everything here is a no-op when no pool is installed or the pool holds a single account.
//! The model client already starts a fresh WebSocket session and resends the full history when
//! the provider hands it another account's `AuthManager`, so nothing else has to be reset.

use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use codex_login::account_pool;
use codex_login::account_pool::AccountUsage;
use codex_login::account_pool::AccountPool;
use codex_login::account_pool::SwitchEvent;
use codex_model_provider_info::WireApi;
use std::sync::Arc;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::RateLimitSnapshot;
use codex_protocol::protocol::WarningEvent;

async fn announce(sess: &Session, turn_context: &TurnContext, event: &SwitchEvent) {
    sess.send_event(
        turn_context,
        EventMsg::Warning(WarningEvent {
            message: event.summary(),
        }),
    )
    .await;
}

/// The pool that serves this turn's provider.
fn pool_for(turn_context: &TurnContext) -> Option<Arc<AccountPool>> {
    let group = match turn_context.config.model_provider.wire_api {
        WireApi::AnthropicMessages => crate::claude_auth::CLAUDE_GROUP,
        _ => "openai",
    };
    account_pool::global_for(group)
}

/// Before each sampling request: move to another account when the current one is about to
/// cross the threshold. `turn_start` is true for the first request of a turn.
pub(crate) async fn before_sampling(sess: &Session, turn_context: &TurnContext, turn_start: bool) {
    let Some(pool) = pool_for(turn_context) else {
        return;
    };
    let thread_id = sess.thread_id.to_string();
    if let Some(event) = pool.select_for_sampling(Some(&thread_id), turn_start) {
        announce(sess, turn_context, &event).await;
    }
}

/// After a limit-type error. Returns true when another account was chosen and the same
/// sampling request should be sent again (with the full history).
pub(crate) async fn recover_from_limit(
    sess: &Session,
    turn_context: &TurnContext,
    err: &CodexErr,
) -> bool {
    let Some(pool) = pool_for(turn_context) else {
        return false;
    };
    let resets_at = match err.details() {
        CodexErrorDetails::UsageLimitReached(limit) => {
            if let Some(snapshot) = limit.rate_limits.as_deref() {
                pool.record_usage_for_current(AccountUsage::from_snapshot(snapshot, now()));
            }
            limit.resets_at.map(|at| at.timestamp())
        }
        CodexErrorDetails::QuotaExceeded
        | CodexErrorDetails::UsageNotIncluded
        | CodexErrorDetails::RateLimitExceeded(_) => None,
        CodexErrorDetails::RetryLimit(limit) if limit.status.as_u16() == 429 => None,
        _ => return false,
    };
    let thread_id = sess.thread_id.to_string();
    match pool.report_limit_reached(Some(&thread_id), resets_at) {
        Some(event) => {
            announce(sess, turn_context, &event).await;
            true
        }
        None => false,
    }
}

/// Rate-limit headers describe the account that served the request.
pub(crate) fn record_rate_limits(turn_context: &TurnContext, snapshot: &RateLimitSnapshot) {
    if let Some(pool) = pool_for(turn_context) {
        pool.record_usage_for_current(AccountUsage::from_snapshot(snapshot, now()));
    }
}

/// Alias of the account serving this turn's provider (never an e-mail address).
pub(crate) fn current_alias(turn_context: &TurnContext) -> Option<String> {
    let pool = pool_for(turn_context)?;
    let id = pool.current_id()?;
    pool.list().into_iter().find(|a| a.id == id).map(|a| a.alias)
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}
