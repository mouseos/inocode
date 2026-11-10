//! `inocode/team/*`. Placeholder: fixture-shaped response (owner: cli-compaction-team).

use super::InocodeResult;
use super::unknown_method;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::InocodeTeamDecision;
use codex_app_server_protocol::InocodeTeamEstimate;
use codex_app_server_protocol::InocodeTeamStatusResponse;

pub(crate) async fn handle(request: ClientRequest) -> InocodeResult {
    match request {
        ClientRequest::InocodeTeamStatus { .. } => Ok(Some(
            InocodeTeamStatusResponse {
                agents: Vec::new(),
                estimate: InocodeTeamEstimate {
                    speedup: 1.0,
                    cost_multiplier: 1.0,
                    decision: InocodeTeamDecision::Single,
                    reason: String::new(),
                },
            }
            .into(),
        )),
        other => Err(unknown_method(other.method_name())),
    }
}
