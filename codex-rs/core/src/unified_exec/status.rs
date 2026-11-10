use serde_json::json;

use super::TerminalSession;
use super::monitor::Health;
use super::monitor::TerminalKind;
use super::monitor::assess;
use super::monitor::tail_lines;

const STATUS_TAIL_LINES: usize = 40;

/// Instant, non-blocking report on a terminal session for the `terminal_status` tool.
///
/// Looking at an anomaly counts as the model's decision to keep waiting: the same
/// anomaly is not announced again and the thresholds are doubled.
pub(crate) async fn terminal_status_report(
    terminal: &TerminalSession,
    declared_kind: Option<&str>,
) -> Result<String, String> {
    let process = &terminal.process;
    if let Some(value) = declared_kind {
        let kind = TerminalKind::parse(value).ok_or_else(|| {
            format!(
                "unknown kind {value:?}; use one of server, watch, build, test, one-shot"
            )
        })?;
        process.with_monitor(|state| state.kind_override = Some(kind));
    }

    let observation = process.observe().await;
    let (kind, declared, scale, command) = process.with_monitor(|state| {
        (
            state.kind(),
            state.kind_override.is_some(),
            state.scale(),
            state.command.clone(),
        )
    });
    let assessment = assess(kind, &observation, scale);
    let notifications = process.with_monitor(|state| {
        state.acknowledge(&assessment);
        state.notifications_sent()
    });

    let note = match assessment.health {
        Health::Running => "Nothing is wrong that can be detected from output alone.",
        Health::Exited => "The process has exited; the output tail below is final.",
        _ => {
            "This is a suspicion based on output only. If the command is expected to behave \
             this way, keep waiting: you will not be told about this again and the thresholds \
             are now doubled. Otherwise send input with write_stdin or terminate the session."
        }
    };
    let report = json!({
        "session_id": terminal.process_id,
        "command": command,
        "state": assessment.health.as_str(),
        "reason": assessment.reason,
        "kind": kind.as_str(),
        "kind_source": if declared { "declared" } else { "inferred_from_command" },
        "elapsed_seconds": observation.elapsed.as_secs(),
        "seconds_since_last_output": observation.idle.as_secs(),
        "output_bytes_total": observation.total_bytes,
        "exit_code": process.exit_code(),
        "anomaly_notifications_sent": notifications,
        "note": note,
        "output_tail": tail_lines(&observation.tail, STATUS_TAIL_LINES),
    });
    serde_json::to_string_pretty(&report).map_err(|err| err.to_string())
}
