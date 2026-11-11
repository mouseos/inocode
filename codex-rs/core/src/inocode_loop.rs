//! InoCode loop instrumentation: measurement hooks and stall detection glue.
//!
//! Every hook is a no-op when its flag in `[inocode]` of `config.toml` is off.

use codex_inocode_flags::InocodeFlags;
use codex_inocode_metrics::MetricEvent;
use codex_protocol::ThreadId;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TurnCompleteEvent;

pub(crate) fn advisor_enabled() -> bool {
    InocodeFlags::load().advisor_enabled
}

pub(crate) fn metrics_enabled() -> bool {
    InocodeFlags::load().metrics_enabled
}

fn record(thread_id: ThreadId, event: MetricEvent) {
    if metrics_enabled() {
        codex_inocode_metrics::record_global(&thread_id.to_string(), event);
    }
}

pub(crate) fn record_turn_complete(thread_id: ThreadId, completed: &TurnCompleteEvent) {
    if let Some(ms) = completed.duration_ms.and_then(|ms| u64::try_from(ms).ok()) {
        record(thread_id, MetricEvent::TurnWallClock { ms });
    }
}

pub(crate) fn record_tokens(thread_id: ThreadId, usage: Option<&TokenUsage>) {
    let Some(usage) = usage else { return };
    record(
        thread_id,
        MetricEvent::Tokens {
            input: u64::try_from(usage.input_tokens).unwrap_or(0),
            output: u64::try_from(usage.output_tokens).unwrap_or(0),
        },
    );
}

pub(crate) fn record_wait_agent_call(thread_id: ThreadId) {
    record(thread_id, MetricEvent::WaitAgentCall);
}

pub(crate) fn record_clarification(thread_id: ThreadId) {
    record(thread_id, MetricEvent::Clarification);
}

/// Stores an agent-to-agent message verbatim under `INOCODE_HOME/messages/<thread>.jsonl`.
pub(crate) fn log_agent_message(
    thread_id: ThreadId,
    from: &str,
    to: &str,
    delivery: &str,
    text: &str,
) {
    let flags = InocodeFlags::load();
    if !flags.protocol_enabled && !flags.peer_messaging_enabled {
        return;
    }
    if text.trim_start().starts_with("ASK") && flags.metrics_enabled {
        record_clarification(thread_id);
    }
    let ts_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    codex_inocode_protocol::log_message_global(
        &thread_id.to_string(),
        &codex_inocode_protocol::LoggedMessage {
            ts_ms,
            from: from.to_string(),
            to: to.to_string(),
            delivery: delivery.to_string(),
            text: text.to_string(),
        },
    );
}

// ---------------------------------------------------------------------------
// Stall detection and the advisor (C5)
// ---------------------------------------------------------------------------

use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use codex_inocode_advisor::ConsultState;
use codex_inocode_advisor::Observation;
use codex_inocode_advisor::Stall;
use codex_inocode_advisor::StallConfig;
use codex_inocode_advisor::StallDetector;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;

const RECENT_ACTIONS: usize = 8;

struct ThreadStall {
    detector: StallDetector,
    recent: VecDeque<String>,
    last_error: String,
}

fn detectors() -> &'static Mutex<HashMap<ThreadId, ThreadStall>> {
    static DETECTORS: OnceLock<Mutex<HashMap<ThreadId, ThreadStall>>> = OnceLock::new();
    DETECTORS.get_or_init(Default::default)
}

/// Stall thresholds from `advisor.*` parameters, read each time a detector is created.
fn stall_config() -> StallConfig {
    use codex_inocode_flags::int_param;
    let get = |key: &str| usize::try_from(int_param(key)).unwrap_or(usize::MAX);
    StallConfig {
        same_failure_k: get("advisor.same_failure_k"),
        repeat_n: get("advisor.repeat_n"),
        alternation_len: get("advisor.alternation_len"),
        no_progress_turns: get("advisor.no_progress_turns"),
    }
}

fn truncate(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

/// Exit code reported in a tool output, if any (`Exit code: N` or `"exit_code": N`).
pub(crate) fn exit_code_in_output(output: &str) -> Option<i32> {
    let lower = output.to_ascii_lowercase();
    for marker in ["exit code:", "\"exit_code\":", "exit_code:"] {
        if let Some(pos) = lower.find(marker) {
            let rest = lower[pos + marker.len()..].trim_start();
            let digits: String = rest
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '-')
                .collect();
            if let Ok(code) = digits.parse() {
                return Some(code);
            }
        }
    }
    None
}

fn test_result_in_output(output: &str) -> Option<String> {
    output
        .lines()
        .find(|line| line.contains("test result:"))
        .map(|line| truncate(line.trim(), 200))
}

fn is_edit_tool(tool: &str) -> bool {
    let tool = tool.to_ascii_lowercase();
    tool.contains("apply_patch") || tool.contains("edit") || tool.contains("write_file")
}

pub(crate) fn build_observation(
    tool: &str,
    args: &str,
    success: bool,
    output: &str,
) -> Observation {
    let exit_code = exit_code_in_output(output);
    let nonzero_exit = exit_code.is_some_and(|c| c != 0);
    Observation {
        tool: tool.to_string(),
        args: truncate(args, 500),
        exit_code,
        error: (!success || nonzero_exit).then(|| truncate(output, 400)),
        changed_files: success && !nonzero_exit && is_edit_tool(tool),
        test_result: test_result_in_output(output),
    }
}

/// Feeds one finished tool call to the stall detector; on a stall, records it and starts a
/// background consultation whose answer arrives in the main agent's next turn input.
pub(crate) fn observe_tool_result(
    session: &Arc<Session>,
    turn: &Arc<TurnContext>,
    tool: &str,
    args: &str,
    success: bool,
    output: &str,
) {
    let flags = InocodeFlags::load();
    if !flags.advisor_enabled {
        return;
    }
    let obs = build_observation(tool, args, success, output);
    let summary = format!("{tool} {}", truncate(args, 160));
    let failed = obs.failed();
    let error_text = obs.error.clone().unwrap_or_default();
    let (stall, tried, last_error) = {
        let Ok(mut map) = detectors().lock() else {
            return;
        };
        let entry = map.entry(session.thread_id).or_insert_with(|| ThreadStall {
            detector: StallDetector::new(stall_config()),
            recent: VecDeque::new(),
            last_error: String::new(),
        });
        entry.recent.push_back(format!(
            "{summary} -> {}",
            if failed { "failed" } else { "ok" }
        ));
        while entry.recent.len() > RECENT_ACTIONS {
            entry.recent.pop_front();
        }
        if failed {
            entry.last_error = error_text;
        }
        let stall = entry.detector.observe(obs);
        (
            stall,
            entry.recent.iter().cloned().collect::<Vec<_>>(),
            entry.last_error.clone(),
        )
    };
    let Some(stall) = stall else { return };
    codex_inocode_advisor::record_global(
        &codex_inocode_flags::inocode_home(),
        codex_inocode_advisor::AdvisorEvent::stall(
            &session.thread_id.to_string(),
            stall.trigger,
            &stall.signature,
            tried.clone(),
        ),
    );
    if flags.metrics_enabled {
        record(
            session.thread_id,
            MetricEvent::StallDetected {
                trigger: stall.trigger.as_str().to_string(),
            },
        );
    }
    // Failure case (knowledge DB): the repeated failure signature is recorded once; the
    // store returns the existing id when a live case with the same signature exists.
    // No-op when `knowledge.enabled` is off.
    {
        let symptom = if last_error.is_empty() {
            tried.last().cloned().unwrap_or_default()
        } else {
            last_error.clone()
        };
        let cwd = crate::compact_inocode::local_cwd(turn);
        if let Err(err) = codex_inocode_knowledge::record_failure_case(
            &cwd,
            &stall.signature,
            &symptom,
            None,
            None,
            None,
            "advisor",
        ) {
            tracing::warn!("recording failure case failed: {err}");
        }
    }
    let session = Arc::clone(session);
    let turn = Arc::clone(turn);
    tokio::spawn(async move {
        run_consultation(session, turn, stall, tried, last_error).await;
    });
}

async fn last_user_text(session: &Session) -> String {
    use codex_protocol::models::ContentItem;
    use codex_protocol::models::ResponseItem;
    let history = session.clone_history().await;
    for item in history.raw_items().rev() {
        if let ResponseItem::Message { role, content, .. } = item
            && role == "user"
        {
            let text: String = content
                .iter()
                .filter_map(|c| match c {
                    ContentItem::InputText { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            if !text.trim().is_empty() {
                return truncate(&text, 1500);
            }
        }
    }
    String::new()
}

async fn run_consultation(
    session: Arc<Session>,
    turn: Arc<TurnContext>,
    stall: Stall,
    tried: Vec<String>,
    last_error: String,
) {
    let goal = last_user_text(&session).await;
    let state = ConsultState {
        goal,
        current_work: tried.last().cloned().unwrap_or_default(),
        tried,
        failure_signature: stall.signature.clone(),
        evidence: if last_error.is_empty() {
            Vec::new()
        } else {
            vec![last_error]
        },
    };
    let model = codex_inocode_flags::string_param("advisor.model");
    let model_for_log = Some(
        model
            .clone()
            .unwrap_or_else(|| turn.model_info().slug.clone()),
    );
    let advice = codex_inocode_advisor::consult(&state, &stall, |prompt| {
        ask_advisor_model(Arc::clone(&session), Arc::clone(&turn), prompt, model)
    })
    .await;
    let thread_id = session.thread_id.to_string();
    let home = codex_inocode_flags::inocode_home();
    match advice {
        Ok(text) => {
            codex_inocode_advisor::record_global(
                &home,
                codex_inocode_advisor::AdvisorEvent::advice(
                    &thread_id,
                    stall.trigger,
                    model_for_log.as_deref(),
                    &text,
                ),
            );
            deliver_advice(&session, text).await
        }
        Err(err) => {
            tracing::warn!("advisor consultation failed: {err}");
            codex_inocode_advisor::record_global(
                &home,
                codex_inocode_advisor::AdvisorEvent::error(
                    &thread_id,
                    stall.trigger,
                    model_for_log.as_deref(),
                    &err.to_string(),
                ),
            );
        }
    }
}

/// Queues the advice as a mailbox note for the main agent's next turn input.
pub(crate) async fn deliver_advice(session: &Arc<Session>, text: String) {
    let author = codex_protocol::AgentPath::root()
        .join("advisor")
        .unwrap_or_else(|_| codex_protocol::AgentPath::root());
    let communication = codex_protocol::protocol::InterAgentCommunication::new(
        author,
        codex_protocol::AgentPath::root(),
        Vec::new(),
        text,
        /*trigger_turn*/ false,
    );
    session
        .input_queue
        .enqueue_mailbox_communication(communication.into(), Default::default())
        .await;
}

const ADVISOR_BASE_INSTRUCTIONS: &str = "You are a debugging advisor for another engineering agent that is stuck. You do not implement anything and you do not edit files. You answer with: other hypotheses for the cause, assumptions that look suspicious, and concrete experiments that would distinguish between the hypotheses. Be brief and specific.";

/// One-shot consultation of the advisor model in an isolated sub-session
/// (collab tools off, approvals never).
async fn ask_advisor_model(
    session: Arc<Session>,
    turn: Arc<TurnContext>,
    prompt: String,
    model: Option<String>,
) -> Result<String, String> {
    use codex_features::Feature;
    use codex_protocol::models::BaseInstructionsProvenance;
    use codex_protocol::protocol::AskForApproval;
    use codex_protocol::protocol::EventMsg;
    use codex_protocol::protocol::SubAgentSource;
    use codex_protocol::user_input::UserInput;

    let mut config = turn.config.as_ref().clone();
    let _ = config.features.disable(Feature::Collab);
    let _ = config.features.disable(Feature::MultiAgentV2);
    config.base_instructions = Some(ADVISOR_BASE_INSTRUCTIONS.to_string());
    config.base_instructions_provenance = Some(BaseInstructionsProvenance::Custom);
    config.permissions.approval_policy =
        crate::config::Constrained::allow_only(AskForApproval::Never);
    config.model = Some(model.unwrap_or_else(|| turn.model_info().slug.clone()));
    let cancel = tokio_util::sync::CancellationToken::new();
    let (_child, io) = crate::codex_delegate::run_codex_thread_one_shot(
        config,
        Arc::clone(&session.services.auth_manager),
        Arc::clone(&session.services.models_manager),
        vec![UserInput::Text {
            text: prompt,
            text_elements: Vec::new(),
        }],
        Arc::clone(&session),
        Arc::clone(&turn),
        cancel,
        SubAgentSource::Other("advisor".to_string()),
        /*final_output_json_schema*/ None,
        /*initial_history*/ None,
    )
    .await
    .map_err(|err| err.to_string())?;
    while let Ok(event) = io.rx_event.recv().await {
        match event.msg {
            EventMsg::TurnComplete(done) => {
                return done
                    .last_agent_message
                    .filter(|text| !text.trim().is_empty())
                    .ok_or_else(|| "advisor returned no message".to_string());
            }
            EventMsg::TurnAborted(_) => return Err("advisor turn aborted".to_string()),
            _ => {}
        }
    }
    Err("advisor channel closed".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_code_is_parsed_from_tool_output() {
        assert_eq!(
            exit_code_in_output("Exit code: 101\nWall time: 1s"),
            Some(101)
        );
        assert_eq!(exit_code_in_output("{\"exit_code\": 2}"), Some(2));
        assert_eq!(exit_code_in_output("hello"), None);
    }

    #[test]
    fn repeated_failures_in_tool_output_fire_the_detector() {
        let mut detector = StallDetector::new(StallConfig::default());
        let mut fired = None;
        for _ in 0..3 {
            fired = detector.observe(build_observation(
                "shell",
                "{\"command\":\"cargo test\"}",
                true,
                "Exit code: 101\nerror: could not compile",
            ));
        }
        assert_eq!(
            fired.map(|s| s.trigger),
            Some(codex_inocode_advisor::Trigger::SameFailure)
        );
    }

    #[test]
    fn successful_edit_counts_as_progress() {
        let obs = build_observation("apply_patch", "{}", true, "Success");
        assert!(obs.changed_files);
        assert!(!obs.failed());
    }
}
