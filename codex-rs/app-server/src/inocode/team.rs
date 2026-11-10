//! `inocode/team/status`: the agent roster and the latest single/multi estimate for a thread.

use super::InocodeResult;
use super::unknown_method;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::InocodeTeamAgent;
use codex_app_server_protocol::InocodeTeamAgentState;
use codex_app_server_protocol::InocodeTeamDecision;
use codex_app_server_protocol::InocodeTeamEstimate;
use codex_app_server_protocol::InocodeTeamStatusResponse;
use codex_inocode_team::AgentState;
use codex_inocode_team::Decision;
use codex_inocode_team::TeamStatus;

pub(crate) fn to_response(status: TeamStatus) -> InocodeTeamStatusResponse {
    InocodeTeamStatusResponse {
        agents: status
            .agents
            .into_iter()
            .map(|a| InocodeTeamAgent {
                id: a.id,
                name: a.name,
                owned_paths: a.owned_paths,
                state: match a.state {
                    AgentState::Running => InocodeTeamAgentState::Running,
                    AgentState::Waiting => InocodeTeamAgentState::Waiting,
                    AgentState::Blocked => InocodeTeamAgentState::Blocked,
                    AgentState::Done => InocodeTeamAgentState::Done,
                },
                tokens: i64::try_from(a.tokens).unwrap_or(i64::MAX),
                started_at: a.started_at,
            })
            .collect(),
        estimate: InocodeTeamEstimate {
            speedup: status.estimate.speedup,
            cost_multiplier: status.estimate.cost_multiplier,
            decision: match status.estimate.decision {
                Decision::Single => InocodeTeamDecision::Single,
                Decision::Multi => InocodeTeamDecision::Multi,
            },
            reason: status.estimate.reason,
        },
    }
}

pub(crate) async fn handle(request: ClientRequest) -> InocodeResult {
    match request {
        ClientRequest::InocodeTeamStatus { params, .. } => {
            let status = codex_inocode_team::roster_for(&params.thread_id).status();
            Ok(Some(to_response(status).into()))
        }
        other => Err(unknown_method(other.method_name())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_inocode_team::TeamAgent;

    #[test]
    fn maps_roster_to_protocol() {
        let roster = codex_inocode_team::roster_for("team-map-test");
        roster.upsert(TeamAgent {
            id: "a".into(),
            name: "api".into(),
            owned_paths: vec!["src".into()],
            state: AgentState::Blocked,
            tokens: 7,
            started_at: "2026-11-11T00:00:00Z".into(),
        });
        let r = to_response(roster.status());
        assert_eq!(r.agents[0].state, InocodeTeamAgentState::Blocked);
        assert_eq!(r.agents[0].tokens, 7);
        assert_eq!(r.estimate.decision, InocodeTeamDecision::Single);
    }
}
