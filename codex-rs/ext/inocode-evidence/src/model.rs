use serde::Deserialize;
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Source {
    pub id: String,
    pub title: String,
    /// `file` / `text` / `law` / `judgment` など。
    pub kind: String,
    /// 原文(バイト列)の SHA-256(16 進)。
    pub hash: String,
    pub retrieved_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// 取り込み元のパス(ファイルの場合)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
}

/// 引用箇所。`start`/`end` は原文中の文字オフセット(char 単位、end は含まない)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Span {
    pub id: String,
    pub source_id: String,
    pub start: usize,
    pub end: usize,
    /// 一字一句の原文。
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimType {
    Fact,
    Interpretation,
    Inference,
    Opinion,
}

impl ClaimType {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "fact" => Some(Self::Fact),
            "interpretation" => Some(Self::Interpretation),
            "inference" => Some(Self::Inference),
            "opinion" => Some(Self::Opinion),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    High,
    Medium,
    Low,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Claim {
    pub id: String,
    pub text: String,
    #[serde(rename = "type")]
    pub claim_type: ClaimType,
    /// `verified` / `reported` / `speculative` / `unverified` / `disputed` / `refuted`。
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<Confidence>,
    #[serde(default)]
    pub span_ids: Vec<String>,
    #[serde(default)]
    pub premise_ids: Vec<String>,
    pub created_by: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingKind {
    QuoteMissing,
    Uncited,
    NovelTerm,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub kind: FindingKind,
    pub text: String,
    /// 文書中の文字オフセット(char 単位)。
    pub position: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphNode {
    pub id: String,
    /// `claim` / `span` / `source`。
    pub kind: String,
    pub label: String,
    pub status: String,
    pub supported: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GraphEdge {
    pub from: String,
    pub to: String,
    /// `supports` / `premise` / `cites`。
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
pub struct Graph {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
}
