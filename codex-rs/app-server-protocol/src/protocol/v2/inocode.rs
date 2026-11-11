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
    #[serde(default)]
    #[ts(optional = nullable)]
    pub created_by: Option<String>,
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
    /// Counts per kind (`rule`, `failure_case`, ...) and per status (`unverified`, ...) in the scope.
    #[serde(default)]
    pub counts: HashMap<String, u32>,
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
    /// The quoted text behind each cited span, with its source title.
    #[serde(default)]
    pub evidence: Vec<InocodeEvidenceClaimEvidence>,
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
    /// The work items the estimate was built from.
    #[serde(default)]
    #[ts(optional = nullable)]
    pub tasks: Option<Vec<InocodeTeamTask>>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeTeamTask {
    pub name: String,
    pub est_seconds: f64,
    pub write_paths: Vec<String>,
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
    pub stalls_detected: i64,
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
    /// Restrict the totals to one thread.
    #[serde(default)]
    #[ts(optional = nullable)]
    pub thread_id: Option<String>,
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

// ------------------------------------------------------ metrics (history etc.)

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum InocodeMetricsBucket {
    Hour,
    Day,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeMetricsHistoryParams {
    /// RFC 3339 timestamp.
    pub since: String,
    pub bucket: InocodeMetricsBucket,
    #[serde(default)]
    #[ts(optional = nullable)]
    pub thread_id: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeMetricsHistoryBucket {
    pub start_ms: i64,
    pub counters: InocodeMetricsCounters,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeMetricsHistoryResponse {
    pub buckets: Vec<InocodeMetricsHistoryBucket>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeMetricsMarkFalsePositiveParams {
    pub thread_id: String,
    /// `guard`, `advisor`, `verifier` or any other label.
    pub source: String,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeMetricsMarkFalsePositiveResponse {}

// ------------------------------------------------------------------ advisor

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum InocodeAdvisorEventKind {
    Stall,
    Advice,
    Error,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case", export_to = "v2/")]
pub enum InocodeAdvisorTrigger {
    SameFailure,
    RepeatedAction,
    Alternation,
    NoProgress,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAdvisorEvent {
    pub ts_ms: i64,
    pub thread_id: String,
    pub kind: InocodeAdvisorEventKind,
    #[serde(default)]
    #[ts(optional = nullable)]
    pub trigger: Option<InocodeAdvisorTrigger>,
    #[serde(default)]
    #[ts(optional = nullable)]
    pub signature: Option<String>,
    #[serde(default)]
    #[ts(optional = nullable)]
    pub recent_actions: Option<Vec<String>>,
    #[serde(default)]
    #[ts(optional = nullable)]
    pub model: Option<String>,
    /// Advice text (`advice`) or error text (`error`).
    #[serde(default)]
    #[ts(optional = nullable)]
    pub text: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAdvisorEventsParams {
    #[serde(default)]
    #[ts(optional = nullable)]
    pub thread_id: Option<String>,
    pub limit: u32,
    #[serde(default)]
    #[ts(optional = nullable)]
    pub cursor: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAdvisorEventsResponse {
    /// Newest first.
    pub events: Vec<InocodeAdvisorEvent>,
    #[ts(optional = nullable)]
    pub next_cursor: Option<String>,
}

/// Notification: one stall / advice / error event (same fields as [`InocodeAdvisorEvent`]).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAdvisorEventNotification {
    pub ts_ms: i64,
    pub thread_id: String,
    pub kind: InocodeAdvisorEventKind,
    #[serde(default)]
    #[ts(optional = nullable)]
    pub trigger: Option<InocodeAdvisorTrigger>,
    #[serde(default)]
    #[ts(optional = nullable)]
    pub signature: Option<String>,
    #[serde(default)]
    #[ts(optional = nullable)]
    pub recent_actions: Option<Vec<String>>,
    #[serde(default)]
    #[ts(optional = nullable)]
    pub model: Option<String>,
    #[serde(default)]
    #[ts(optional = nullable)]
    pub text: Option<String>,
}

// ----------------------------------------------------------------- messages

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[ts(export_to = "v2/")]
pub enum InocodeMessageKind {
    #[serde(rename = "ASK")]
    #[ts(rename = "ASK")]
    Ask,
    #[serde(rename = "ANSWER")]
    #[ts(rename = "ANSWER")]
    Answer,
    #[serde(rename = "BLOCKED")]
    #[ts(rename = "BLOCKED")]
    Blocked,
    #[serde(rename = "RESULT")]
    #[ts(rename = "RESULT")]
    Result,
    #[serde(rename = "HANDOFF")]
    #[ts(rename = "HANDOFF")]
    Handoff,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[ts(export_to = "v2/")]
pub enum InocodeMessageConfidence {
    #[serde(rename = "high")]
    #[ts(rename = "high")]
    High,
    #[serde(rename = "med")]
    #[ts(rename = "med")]
    Med,
    #[serde(rename = "low")]
    #[ts(rename = "low")]
    Low,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeMessageField {
    pub key: String,
    pub value: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeParsedMessage {
    pub kind: InocodeMessageKind,
    pub conf: InocodeMessageConfidence,
    pub fields: Vec<InocodeMessageField>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeMessage {
    pub ts_ms: i64,
    /// Root thread of the conversation this message belongs to.
    pub thread_id: String,
    pub from: String,
    pub to: String,
    /// `send_message`, `followup_task`, `spawn`, `final`.
    pub delivery: String,
    /// Verbatim text.
    pub text: String,
    /// Present when the text is a valid telegraphic message.
    #[serde(default)]
    #[ts(optional = nullable)]
    pub parsed: Option<InocodeParsedMessage>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeMessagesListParams {
    /// Root thread of the conversation.
    pub thread_id: String,
    #[serde(default)]
    #[ts(optional = nullable)]
    pub since_ts_ms: Option<i64>,
    pub limit: u32,
    #[serde(default)]
    #[ts(optional = nullable)]
    pub cursor: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeMessagesListResponse {
    /// Oldest first.
    pub messages: Vec<InocodeMessage>,
    #[ts(optional = nullable)]
    pub next_cursor: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeMessagesAppendedNotification {
    pub thread_id: String,
    pub message: InocodeMessage,
}

// --------------------------------------------------------------- compaction

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum InocodeCompactionEventKind {
    Completed,
    Failed,
    BreakerTripped,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeCompactionEvent {
    pub thread_id: String,
    pub kind: InocodeCompactionEventKind,
    pub auto: bool,
    pub reason: String,
    pub reattached_files: Vec<String>,
    pub pinned_rule_ids: Vec<String>,
    pub consecutive_failures: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeCompactionEventNotification {
    pub thread_id: String,
    pub kind: InocodeCompactionEventKind,
    pub auto: bool,
    pub reason: String,
    pub reattached_files: Vec<String>,
    pub pinned_rule_ids: Vec<String>,
    pub consecutive_failures: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeCompactionStateParams {
    pub thread_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeCompactionStateResponse {
    pub consecutive_failures: u32,
    pub max_failures: u32,
    pub breaker_tripped: bool,
    #[serde(default)]
    #[ts(optional = nullable)]
    pub last_event: Option<InocodeCompactionEvent>,
}

// --------------------------------------------------------------- toolOutput

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeToolOutputItem {
    pub id: String,
    pub ts_ms: i64,
    pub bytes: i64,
    pub approx_tokens: i64,
    #[serde(default)]
    #[ts(optional = nullable)]
    pub tool: Option<String>,
    #[serde(default)]
    #[ts(optional = nullable)]
    pub thread_id: Option<String>,
    #[serde(default)]
    #[ts(optional = nullable)]
    pub call_id: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeToolOutputListParams {
    pub limit: u32,
    #[serde(default)]
    #[ts(optional = nullable)]
    pub cursor: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeToolOutputListResponse {
    /// Newest first.
    pub items: Vec<InocodeToolOutputItem>,
    #[ts(optional = nullable)]
    pub next_cursor: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeToolOutputReadParams {
    pub id: String,
    #[serde(default)]
    #[ts(optional = nullable)]
    pub offset: Option<u32>,
    #[serde(default)]
    #[ts(optional = nullable)]
    pub limit: Option<u32>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeToolOutputReadResponse {
    pub id: String,
    /// Total size of the stored output in bytes.
    pub bytes: i64,
    pub text: String,
    pub truncated: bool,
}

// ------------------------------------------- guard / pinned / handoff (S1, S2)

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum InocodeGuardResult {
    Blocked,
    Warned,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeGuardEvent {
    pub ts_ms: i64,
    pub thread_id: String,
    #[ts(optional = nullable)]
    pub turn_id: Option<String>,
    #[ts(optional = nullable)]
    pub tool_use_id: Option<String>,
    pub tool: String,
    /// The command or the paths the rule matched.
    pub target: String,
    pub severity: InocodeRuleSeverity,
    pub result: InocodeGuardResult,
    pub rule_id: String,
    pub rule_title: String,
    pub reason: String,
    pub scope: InocodeKnowledgeScope,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeGuardListParams {
    #[ts(optional = nullable)]
    pub cwd: Option<String>,
    #[ts(optional = nullable)]
    pub thread_id: Option<String>,
    pub limit: u32,
    #[ts(optional = nullable)]
    pub cursor: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeGuardListResponse {
    pub events: Vec<InocodeGuardEvent>,
    #[ts(optional = nullable)]
    pub next_cursor: Option<String>,
}

/// Same shape as `InocodeGuardEvent`; sent when a guard rule fires.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeGuardFiredNotification {
    pub ts_ms: i64,
    pub thread_id: String,
    #[ts(optional = nullable)]
    pub turn_id: Option<String>,
    #[ts(optional = nullable)]
    pub tool_use_id: Option<String>,
    pub tool: String,
    pub target: String,
    pub severity: InocodeRuleSeverity,
    pub result: InocodeGuardResult,
    pub rule_id: String,
    pub rule_title: String,
    pub reason: String,
    pub scope: InocodeKnowledgeScope,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeKnowledgePinnedParams {
    pub cwd: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeKnowledgePinnedRule {
    pub id: String,
    pub title: String,
    #[ts(optional = nullable)]
    pub reason: Option<String>,
    pub scope: InocodeKnowledgeScope,
    pub status: InocodeKnowledgeStatus,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeKnowledgePinnedResponse {
    pub rules: Vec<InocodeKnowledgePinnedRule>,
    /// The re-injected text itself.
    pub text: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeKnowledgeHandoffPreviewParams {
    pub cwd: String,
    pub task: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeKnowledgeHandoffPreviewResponse {
    pub text: String,
}

/// A knowledge record (such as a failure case) was recorded automatically.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeKnowledgeRecordedNotification {
    pub id: String,
    pub kind: InocodeKnowledgeKind,
    pub scope: InocodeKnowledgeScope,
    pub title: String,
}

// ------------------------------------------------------------------- params (S3)

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum InocodeParamKind {
    Int,
    Float,
    String,
    Enum,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(untagged)]
#[ts(export_to = "v2/")]
pub enum InocodeParamValue {
    Number(f64),
    Text(String),
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeParam {
    pub key: String,
    pub kind: InocodeParamKind,
    pub value: InocodeParamValue,
    pub default: InocodeParamValue,
    #[ts(optional = nullable)]
    pub options: Option<Vec<String>>,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeParamsReadParams {}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeParamsReadResponse {
    pub params: Vec<InocodeParam>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeParamsWriteParams {
    pub key: String,
    /// `null` removes the key and returns it to its default.
    #[serde(default)]
    #[ts(optional = nullable)]
    pub value: Option<InocodeParamValue>,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeParamsWriteResponse {}

// ------------------------------------------------------- evidence writes (S9)

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeEvidenceImportSourceParams {
    /// Omitted: `fileName`, else the first line of `text`.
    #[serde(default)]
    #[ts(optional = nullable)]
    pub title: Option<String>,
    #[serde(default)]
    #[ts(optional = nullable)]
    pub file_name: Option<String>,
    pub kind: String,
    #[ts(optional = nullable)]
    pub text: Option<String>,
    #[ts(optional = nullable)]
    pub path: Option<String>,
    #[ts(optional = nullable)]
    pub version: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeEvidenceImportSourceResponse {
    pub id: String,
    pub hash: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeEvidenceAddQuoteParams {
    pub source_id: String,
    /// The verbatim text to cite; or give `start` and `end` (character offsets).
    #[ts(optional = nullable)]
    pub quote: Option<String>,
    #[ts(optional = nullable)]
    pub start: Option<u32>,
    #[ts(optional = nullable)]
    pub end: Option<u32>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeEvidenceAddQuoteResponse {
    pub span_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeEvidenceAddClaimParams {
    pub text: String,
    #[serde(rename = "type")]
    #[ts(rename = "type")]
    pub type_: String,
    pub confidence: InocodeKnowledgeConfidence,
    pub span_ids: Vec<String>,
    pub premise_ids: Vec<String>,
    pub created_by: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeEvidenceAddClaimResponse {
    pub id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeEvidenceSetClaimStatusParams {
    pub id: String,
    pub status: String,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeEvidenceSetClaimStatusResponse {}

// ------------------------------------------- knowledge/evidence UX additions (doc 19)

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeEvidenceSpan {
    pub id: String,
    pub source_id: String,
    pub quote: String,
    /// Character offsets into the source text; `end` is exclusive.
    pub start: u32,
    pub end: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeEvidenceListSpansParams {
    pub cwd: String,
    #[ts(optional = nullable)]
    pub source_id: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeEvidenceListSpansResponse {
    pub spans: Vec<InocodeEvidenceSpan>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeEvidenceRemoveQuoteParams {
    pub span_id: String,
    #[ts(optional = nullable)]
    pub cwd: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeEvidenceRemoveQuoteResponse {}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeEvidenceUpdateSourceParams {
    pub id: String,
    #[ts(optional = nullable)]
    pub title: Option<String>,
    #[ts(optional = nullable)]
    pub kind: Option<String>,
    #[ts(optional = nullable)]
    pub version: Option<String>,
    #[ts(optional = nullable)]
    pub cwd: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeEvidenceUpdateSourceResponse {
    pub source: InocodeEvidenceSource,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeKnowledgeDeleteParams {
    pub id: String,
    pub scope: InocodeKnowledgeScope,
    pub cwd: String,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeKnowledgeDeleteResponse {}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeEvidenceClaimEvidence {
    pub span_id: String,
    pub source_title: String,
    pub quote: String,
}
