//! `inocode/metrics/*`. Placeholder: fixture-shaped response (owner: cli-loop / metrics).

use super::InocodeResult;
use super::unknown_method;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::InocodeMetricsCounters;
use codex_app_server_protocol::InocodeMetricsSummaryResponse;

pub(crate) async fn handle(request: ClientRequest) -> InocodeResult {
    match request {
        ClientRequest::InocodeMetricsSummary { .. } => Ok(Some(
            InocodeMetricsSummaryResponse {
                counters: InocodeMetricsCounters {
                    wall_clock_ms: 0,
                    tokens_in: 0,
                    tokens_out: 0,
                    wait_agent_calls: 0,
                    clarifications: 0,
                    false_positives: 0,
                },
                per_thread: None,
            }
            .into(),
        )),
        other => Err(unknown_method(other.method_name())),
    }
}
