//! Experimental `inocode/*` extension APIs: knowledge, evidence, team, metrics, flags.
//! Contract: ref/docs/09-overnight-contract.md, chapter 4.

use crate::JsonSchema;
use crate::TS;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;

// ---------------------------------------------------------------- knowledge

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum InocodeKnowledgeScope {
    Project,
    Global,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case", export_to = "v2/")]
pub enum InocodeKnowledgeKind {
    Rule,
    FailureCase,
    Issue,
    Decision,
    Fact,
    Contract,
    Procedure,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum InocodeKnowledgeStatus {
    Unverified,
    Verified,
    Superseded,
    Retracted,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum InocodeKnowledgeConfidence {
    High,
    Medium,
    Low,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum InocodeRuleSeverity {
    Block,
    Warn,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum InocodeRuleEnforcedBy {
    Prompt,
    Hook,
    Execpolicy,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeRuleMatch {
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub commands: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeKnowledgeSummary {
    pub id: String,
    pub kind: InocodeKnowledgeKind,
    pub title: String,
    pub summary: String,
    pub scope: InocodeKnowledgeScope,
    pub status: InocodeKnowledgeStatus,
    pub confidence: InocodeKnowledgeConfidence,
    pub updated_at: String,
}

/// A full knowledge record (front-matter plus body). Kind-specific fields are
/// present only for that kind.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeKnowledgeRecord {
    pub id: String,
    pub kind: InocodeKnowledgeKind,
    pub title: String,
    pub scope: InocodeKnowledgeScope,
    pub status: InocodeKnowledgeStatus,
    #[ts(optional)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub confidence: InocodeKnowledgeConfidence,
    pub created_at: String,
    pub updated_at: String,
    pub created_by: String,
    #[ts(optional)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<String>,
    // rule
    #[ts(optional)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<InocodeRuleSeverity>,
    #[serde(rename = "match")]
    #[ts(rename = "match", optional)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub match_: Option<InocodeRuleMatch>,
    #[ts(optional)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[ts(optional)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enforced_by: Option<InocodeRuleEnforcedBy>,
    #[ts(optional)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_failure: Option<String>,
    // failure_case
    #[ts(optional)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    #[ts(optional)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symptom: Option<String>,
    #[ts(optional)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_cause: Option<String>,
    #[ts(optional)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub do_not: Option<String>,
    #[ts(optional)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub what_worked: Option<String>,
    #[ts(optional)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<Vec<String>>,
    pub body: String,
}

/// A record to write. Omitted fields get defaults (`id` is assigned, status
/// `unverified`, timestamps now).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeKnowledgeRecordInput {
    #[ts(optional)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub kind: InocodeKnowledgeKind,
    pub title: String,
    #[ts(optional)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<InocodeKnowledgeScope>,
    #[ts(optional)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<InocodeKnowledgeStatus>,
    #[ts(optional)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[ts(optional)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<InocodeKnowledgeConfidence>,
    #[ts(optional)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
    #[ts(optional)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<String>,
    #[ts(optional)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<InocodeRuleSeverity>,
    #[serde(rename = "match")]
    #[ts(rename = "match", optional)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub match_: Option<InocodeRuleMatch>,
    #[ts(optional)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[ts(optional)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enforced_by: Option<InocodeRuleEnforcedBy>,
    #[ts(optional)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_failure: Option<String>,
    #[ts(optional)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    #[ts(optional)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symptom: Option<String>,
    #[ts(optional)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_cause: Option<String>,
    #[ts(optional)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub do_not: Option<String>,
    #[ts(optional)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub what_worked: Option<String>,
    #[ts(optional)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<Vec<String>>,
    #[serde(default)]
    pub body: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeKnowledgeListParams {
    pub scope: InocodeKnowledgeScope,
    pub cwd: String,
    #[ts(optional = nullable)]
    pub kind: Option<InocodeKnowledgeKind>,
    #[ts(optional = nullable)]
    pub status: Option<InocodeKnowledgeStatus>,
    #[ts(optional = nullable)]
    pub query: Option<String>,
    pub limit: u32,
    #[ts(optional = nullable)]
    pub cursor: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeKnowledgeListResponse {
    pub records: Vec<InocodeKnowledgeSummary>,
    #[ts(optional = nullable)]
    pub next_cursor: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeKnowledgeReadParams {
    pub id: String,
    pub scope: InocodeKnowledgeScope,
    pub cwd: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeKnowledgeReadResponse {
    pub record: InocodeKnowledgeRecord,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeKnowledgeWriteParams {
    pub scope: InocodeKnowledgeScope,
    pub cwd: String,
    pub record: InocodeKnowledgeRecordInput,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeKnowledgeWriteResponse {
    pub id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeKnowledgeSetStatusParams {
    pub id: String,
    pub scope: InocodeKnowledgeScope,
    pub cwd: String,
    pub status: InocodeKnowledgeStatus,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeKnowledgeSetStatusResponse {}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeKnowledgeSearchParams {
    #[ts(optional = nullable)]
    pub scope: Option<InocodeKnowledgeScope>,
    pub cwd: String,
    pub query: String,
    #[ts(optional = nullable)]
    pub kind: Option<InocodeKnowledgeKind>,
    pub limit: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeKnowledgeSearchResponse {
    pub records: Vec<InocodeKnowledgeSummary>,
    #[ts(optional = nullable)]
    pub next_cursor: Option<String>,
}

// ----------------------------------------------------------------- evidence

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeEvidenceSource {
    pub id: String,
    pub title: String,
    pub kind: String,
    pub hash: String,
    pub retrieved_at: String,
    #[ts(optional)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeEvidenceListSourcesParams {
    pub cwd: String,
    pub limit: u32,
    #[ts(optional = nullable)]
    pub cursor: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeEvidenceListSourcesResponse {
    pub sources: Vec<InocodeEvidenceSource>,
    #[ts(optional = nullable)]
    pub next_cursor: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeEvidenceReadSourceParams {
    pub id: String,
    pub cwd: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeEvidenceReadSourceResponse {
    pub source: InocodeEvidenceSource,
    /// Original text, unchanged.
    pub text: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeEvidenceClaim {
    pub id: String,
    pub text: String,
    #[serde(rename = "type")]
    #[ts(rename = "type")]
    pub type_: String,
    pub status: String,
    pub confidence: InocodeKnowledgeConfidence,
    pub span_ids: Vec<String>,
    pub premise_ids: Vec<String>,
    pub created_by: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeEvidenceListClaimsParams {
    pub cwd: String,
    #[ts(optional = nullable)]
    pub status: Option<String>,
    pub limit: u32,
    #[ts(optional = nullable)]
    pub cursor: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeEvidenceListClaimsResponse {
    pub claims: Vec<InocodeEvidenceClaim>,
    #[ts(optional = nullable)]
    pub next_cursor: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum InocodeEvidenceNodeKind {
    Claim,
    Span,
    Source,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum InocodeEvidenceEdgeKind {
    Supports,
    Premise,
    Cites,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeEvidenceGraphNode {
    pub id: String,
    pub kind: InocodeEvidenceNodeKind,
    pub label: String,
    pub status: String,
    /// `false` marks a node with no supporting evidence.
    pub supported: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeEvidenceGraphEdge {
    pub from: String,
    pub to: String,
    pub kind: InocodeEvidenceEdgeKind,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeEvidenceGraphParams {
    pub cwd: String,
    #[ts(optional = nullable)]
    pub root_claim_id: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeEvidenceGraphResponse {
    pub nodes: Vec<InocodeEvidenceGraphNode>,
    pub edges: Vec<InocodeEvidenceGraphEdge>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeEvidenceVerifyDocumentParams {
    pub cwd: String,
    pub text: String,
    pub source_ids: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case", export_to = "v2/")]
pub enum InocodeEvidenceFindingKind {
    QuoteMissing,
    Uncited,
    NovelTerm,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeEvidenceTextPosition {
    pub start: u32,
    pub end: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeEvidenceFinding {
    pub kind: InocodeEvidenceFindingKind,
    pub text: String,
    pub position: InocodeEvidenceTextPosition,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeEvidenceVerifyDocumentResponse {
    pub findings: Vec<InocodeEvidenceFinding>,
}

// --------------------------------------------------------------------- team

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum InocodeTeamAgentState {
    Running,
    Waiting,
    Blocked,
    Done,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum InocodeTeamDecision {
    Single,
    Multi,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeTeamAgent {
    pub id: String,
    pub name: String,
    pub owned_paths: Vec<String>,
    pub state: InocodeTeamAgentState,
    pub tokens: i64,
    pub started_at: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeTeamEstimate {
    pub speedup: f64,
    pub cost_multiplier: f64,
    pub decision: InocodeTeamDecision,
    pub reason: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeTeamStatusParams {
    pub thread_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeTeamStatusResponse {
    pub agents: Vec<InocodeTeamAgent>,
    pub estimate: InocodeTeamEstimate,
}

/// Notification: the team of `thread_id` changed. Carries only the agents that changed.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeTeamUpdatedNotification {
    pub thread_id: String,
    pub agents: Vec<InocodeTeamAgent>,
    pub estimate: InocodeTeamEstimate,
}

// ------------------------------------------------------------------ metrics

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeMetricsCounters {
    pub wall_clock_ms: i64,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub wait_agent_calls: i64,
    pub clarifications: i64,
    pub false_positives: i64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeMetricsThread {
    pub thread_id: String,
    pub counters: InocodeMetricsCounters,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeMetricsSummaryParams {
    /// RFC 3339 timestamp.
    pub since: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeMetricsSummaryResponse {
    pub counters: InocodeMetricsCounters,
    #[ts(optional = nullable)]
    pub per_thread: Option<Vec<InocodeMetricsThread>>,
}

// -------------------------------------------------------------------- flags

#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeFlagsReadParams {}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeFlagsReadResponse {
    /// Keys such as `metrics.enabled`.
    pub flags: HashMap<String, bool>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeFlagsWriteParams {
    pub key: String,
    pub value: bool,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeFlagsWriteResponse {}
