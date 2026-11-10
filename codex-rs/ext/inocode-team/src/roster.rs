use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;

use serde::Deserialize;
use serde::Serialize;

use crate::estimate::Estimate;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentState {
    Running,
    Waiting,
    Blocked,
    Done,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamAgent {
    pub id: String,
    pub name: String,
    pub owned_paths: Vec<String>,
    pub state: AgentState,
    pub tokens: u64,
    /// RFC 3339 timestamp.
    pub started_at: String,
}

/// Response body of `inocode/team/status`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamStatus {
    pub agents: Vec<TeamAgent>,
    pub estimate: Estimate,
}

/// Thread-safe roster of agents for one thread.
#[derive(Debug, Default)]
pub struct TeamRoster {
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    agents: Vec<TeamAgent>,
    estimate: Option<Estimate>,
}

impl TeamRoster {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn upsert(&self, agent: TeamAgent) {
        let mut g = self.lock();
        match g.agents.iter_mut().find(|a| a.id == agent.id) {
            Some(slot) => *slot = agent,
            None => g.agents.push(agent),
        }
    }

    pub fn set_state(&self, id: &str, state: AgentState) -> bool {
        let mut g = self.lock();
        match g.agents.iter_mut().find(|a| a.id == id) {
            Some(a) => {
                a.state = state;
                true
            }
            None => false,
        }
    }

    pub fn add_tokens(&self, id: &str, tokens: u64) {
        let mut g = self.lock();
        if let Some(a) = g.agents.iter_mut().find(|a| a.id == id) {
            a.tokens = a.tokens.saturating_add(tokens);
        }
    }

    pub fn set_estimate(&self, estimate: Estimate) {
        self.lock().estimate = Some(estimate);
    }

    pub fn status(&self) -> TeamStatus {
        let g = self.lock();
        TeamStatus {
            agents: g.agents.clone(),
            estimate: g
                .estimate
                .clone()
                .unwrap_or_else(|| Estimate::single("no estimate yet")),
        }
    }
}

/// Process-wide roster of `thread_id`, shared by the agent tools (writers) and
/// `inocode/team/status` (reader).
pub fn roster_for(thread_id: &str) -> Arc<TeamRoster> {
    static MAP: OnceLock<Mutex<HashMap<String, Arc<TeamRoster>>>> = OnceLock::new();
    let mut map = MAP
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    map.entry(thread_id.to_string()).or_default().clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_returns_same_roster() {
        assert!(Arc::ptr_eq(&roster_for("x"), &roster_for("x")));
    }

    #[test]
    fn status_serializes_to_contract_shape() {
        let r = TeamRoster::default();
        r.upsert(TeamAgent {
            id: "a1".into(),
            name: "api".into(),
            owned_paths: vec!["src/api".into()],
            state: AgentState::Running,
            tokens: 10,
            started_at: "2026-11-11T00:00:00Z".into(),
        });
        r.add_tokens("a1", 5);
        assert!(r.set_state("a1", AgentState::Blocked));
        let v = serde_json::to_value(r.status()).unwrap();
        assert_eq!(v["agents"][0]["ownedPaths"][0], "src/api");
        assert_eq!(v["agents"][0]["state"], "blocked");
        assert_eq!(v["agents"][0]["tokens"], 15);
        assert_eq!(v["estimate"]["decision"], "single");
        assert!(v["estimate"]["costMultiplier"].is_number());
    }
}
