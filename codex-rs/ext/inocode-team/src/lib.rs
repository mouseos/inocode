//! Multi-agent ("team") foundation: contract files, speed-up estimate with
//! automatic single/multi selection, and the roster behind `inocode/team/status`.

mod auto_select;
mod contract;
mod estimate;
mod roster;

pub use auto_select::detect_tasks;
pub use auto_select::recommend_for_input;
pub use contract::contract_instruction;
pub use contract::read_contracts;
pub use estimate::Decision;
pub use estimate::Estimate;
pub use estimate::EstimateConfig;
pub use estimate::Task;
pub use estimate::estimate;
pub use estimate::recommendation_text;
pub use roster::AgentState;
pub use roster::TeamAgent;
pub use roster::TeamRoster;
pub use roster::roster_for;
pub use roster::TeamStatus;
