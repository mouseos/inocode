//! InoCode 根拠データベース: 原資料 → 引用箇所 → 主張。
//!
//! 保存先は `<INOCODE_HOME>/evidence/`。原資料の原文は `sources/<id>.txt` に
//! 変更せず保存し、メタ情報(SHA-256、取得日時、版)は `sources/<id>.json`。
//! 検証(`verify_document`)は機械的なものだけで、LLM は呼ばない。検出して知らせるだけで、
//! 出力は止めない。

mod model;
mod store;
mod verify;

pub use model::{
    Claim, ClaimType, Confidence, Finding, FindingKind, Graph, GraphEdge, GraphNode, Source, Span,
};
pub use store::{NewClaim, Store};
pub use verify::verify_document;

use std::path::PathBuf;

/// `<INOCODE_HOME>/evidence`。
pub fn default_root() -> PathBuf {
    codex_inocode_flags::inocode_home().join("evidence")
}

/// 既定の保存先を使う Store。
pub fn default_store() -> Store {
    Store::new(default_root())
}

/// `evidence.enabled` フラグ。
pub fn enabled() -> bool {
    codex_inocode_flags::load().evidence_enabled
}
