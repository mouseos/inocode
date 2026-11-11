//! `inocode/knowledge/*`: backed by the `codex-inocode-knowledge` crate
//! (one Markdown file per record; the files are the source of truth).

use super::InocodeResult;
use super::unknown_method;
use crate::error_code::internal_error;
use crate::error_code::invalid_params;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::InocodeKnowledgeHandoffPreviewResponse;
use codex_app_server_protocol::InocodeKnowledgeKind;
use codex_app_server_protocol::InocodeKnowledgeListResponse;
use codex_app_server_protocol::InocodeKnowledgePinnedResponse;
use codex_app_server_protocol::InocodeKnowledgePinnedRule;
use codex_app_server_protocol::InocodeKnowledgeReadResponse;
use codex_app_server_protocol::InocodeKnowledgeScope;
use codex_app_server_protocol::InocodeKnowledgeSearchResponse;
use codex_app_server_protocol::InocodeKnowledgeSetStatusResponse;
use codex_app_server_protocol::InocodeKnowledgeStatus;
use codex_app_server_protocol::InocodeKnowledgeSummary;
use codex_app_server_protocol::InocodeKnowledgeWriteResponse;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_inocode_knowledge as kb;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::path::PathBuf;

const DEFAULT_LIMIT: usize = 50;

fn store(cwd: &str) -> kb::Store {
    kb::store_for(&PathBuf::from(cwd))
}

/// The protocol types and the crate types share the same camelCase JSON shape,
/// so values are bridged through JSON.
fn convert<T: Serialize, U: DeserializeOwned>(
    value: &T,
    patch: impl FnOnce(&mut serde_json::Map<String, Value>),
) -> Result<U, JSONRPCErrorError> {
    let mut v = serde_json::to_value(value).map_err(|e| internal_error(e.to_string()))?;
    if let Value::Object(m) = &mut v {
        m.retain(|_, x| !x.is_null());
        patch(m);
    }
    serde_json::from_value(v).map_err(|e| internal_error(e.to_string()))
}

fn scope(s: InocodeKnowledgeScope) -> kb::Scope {
    match s {
        InocodeKnowledgeScope::Project => kb::Scope::Project,
        InocodeKnowledgeScope::Global => kb::Scope::Global,
    }
}

fn kind(k: InocodeKnowledgeKind) -> Option<kb::Kind> {
    serde_json::to_value(k)
        .ok()
        .and_then(|v| v.as_str().and_then(kb::Kind::parse))
}

fn status(s: InocodeKnowledgeStatus) -> Option<kb::Status> {
    serde_json::to_value(s)
        .ok()
        .and_then(|v| v.as_str().and_then(kb::Status::parse))
}

fn summaries(
    rows: Vec<kb::SummaryRow>,
    has_more: bool,
    offset: usize,
) -> Result<(Vec<InocodeKnowledgeSummary>, Option<String>), JSONRPCErrorError> {
    let n = rows.len();
    let out = rows
        .into_iter()
        .map(|r| {
            convert(&r.record, |m| {
                m.insert("summary".into(), Value::String(r.summary.clone()));
                m.entry("confidence").or_insert(Value::String("medium".into()));
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok((out, has_more.then(|| (offset + n).to_string())))
}

fn list(
    cwd: &str,
    scope_: Option<InocodeKnowledgeScope>,
    kind_: Option<InocodeKnowledgeKind>,
    status_: Option<InocodeKnowledgeStatus>,
    query: Option<String>,
    limit: u32,
    cursor: Option<String>,
) -> Result<(Vec<InocodeKnowledgeSummary>, Option<String>), JSONRPCErrorError> {
    let offset = cursor.and_then(|c| c.parse::<usize>().ok()).unwrap_or(0);
    let limit = if limit == 0 {
        DEFAULT_LIMIT
    } else {
        limit as usize
    };
    let (rows, more) = store(cwd).list(&kb::ListQuery {
        scope: scope_.map(scope),
        kind: kind_.and_then(kind),
        status: status_.and_then(status),
        query: query.filter(|q| !q.trim().is_empty()),
        limit,
        offset,
    });
    summaries(rows, more, offset)
}

pub(crate) async fn handle(request: ClientRequest) -> InocodeResult {
    match request {
        ClientRequest::InocodeKnowledgeList { params, .. } => {
            let (records, next_cursor) = list(
                &params.cwd,
                Some(params.scope),
                params.kind,
                params.status,
                params.query,
                params.limit,
                params.cursor,
            )?;
            Ok(Some(
                InocodeKnowledgeListResponse {
                    records,
                    next_cursor,
                }
                .into(),
            ))
        }
        ClientRequest::InocodeKnowledgeRead { params, .. } => {
            let rec = store(&params.cwd)
                .read(scope(params.scope), &params.id)
                .ok_or_else(|| {
                    invalid_params(format!("knowledge record not found: {}", params.id))
                })?;
            let record = convert(&rec, |m| {
                m.entry("confidence").or_insert(Value::String("medium".into()));
                m.entry("createdBy").or_insert(Value::String("unknown".into()));
                m.entry("createdAt").or_insert(Value::String(String::new()));
                m.entry("updatedAt").or_insert(Value::String(String::new()));
            })?;
            Ok(Some(InocodeKnowledgeReadResponse { record }.into()))
        }
        ClientRequest::InocodeKnowledgeWrite { params, .. } => {
            let scope_ = scope(params.scope);
            let rec: kb::Record = convert(&params.record, |m| {
                m.insert(
                    "scope".into(),
                    Value::String(
                        if scope_ == kb::Scope::Global {
                            "global"
                        } else {
                            "project"
                        }
                        .into(),
                    ),
                );
            })?;
            let id = store(&params.cwd)
                .write(rec)
                .map_err(|e| internal_error(format!("knowledge write failed: {e}")))?;
            Ok(Some(InocodeKnowledgeWriteResponse { id }.into()))
        }
        ClientRequest::InocodeKnowledgeSetStatus { params, .. } => {
            let st = status(params.status)
                .ok_or_else(|| invalid_params("unknown status".to_string()))?;
            store(&params.cwd)
                .set_status(scope(params.scope), &params.id, st)
                .map_err(|e| invalid_params(format!("knowledge setStatus failed: {e}")))?;
            Ok(Some(InocodeKnowledgeSetStatusResponse {}.into()))
        }
        ClientRequest::InocodeKnowledgeSearch { params, .. } => {
            let (records, next_cursor) = list(
                &params.cwd,
                params.scope,
                params.kind,
                None,
                Some(params.query),
                params.limit,
                None,
            )?;
            Ok(Some(
                InocodeKnowledgeSearchResponse {
                    records,
                    next_cursor,
                }
                .into(),
            ))
        }
        ClientRequest::InocodeKnowledgePinned { params, .. } => {
            let cwd = PathBuf::from(&params.cwd);
            let rules = kb::pinned_rules(&cwd)
                .into_iter()
                .map(|r| InocodeKnowledgePinnedRule {
                    id: r.id,
                    title: r.title,
                    reason: r.reason.filter(|s| !s.is_empty()),
                    scope: match r.scope {
                        kb::Scope::Project => InocodeKnowledgeScope::Project,
                        kb::Scope::Global => InocodeKnowledgeScope::Global,
                    },
                    status: match r.status {
                        kb::Status::Unverified => InocodeKnowledgeStatus::Unverified,
                        kb::Status::Verified => InocodeKnowledgeStatus::Verified,
                        kb::Status::Superseded => InocodeKnowledgeStatus::Superseded,
                        kb::Status::Retracted => InocodeKnowledgeStatus::Retracted,
                    },
                })
                .collect();
            Ok(Some(
                InocodeKnowledgePinnedResponse {
                    rules,
                    text: kb::pinned_rules_text(&cwd),
                }
                .into(),
            ))
        }
        ClientRequest::InocodeKnowledgeHandoffPreview { params, .. } => {
            let text = kb::build_handoff_pack(&PathBuf::from(&params.cwd), &params.task);
            Ok(Some(InocodeKnowledgeHandoffPreviewResponse { text }.into()))
        }
        other => Err(unknown_method(other.method_name())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_app_server_protocol::InocodeKnowledgeListParams;
    use codex_app_server_protocol::InocodeKnowledgeRecordInput;
    use codex_app_server_protocol::InocodeKnowledgeWriteParams;
    use codex_app_server_protocol::RequestId;

    #[tokio::test]
    async fn write_then_list_and_read() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_string_lossy().into_owned();
        let input: InocodeKnowledgeRecordInput = serde_json::from_value(serde_json::json!({
            "kind": "rule",
            "title": "force push 禁止",
            "severity": "block",
            "match": {"commands": ["git push --force"]},
            "reason": "履歴が消える",
            "enforcedBy": "hook",
            "body": "本文"
        }))
        .unwrap();
        let w = handle(ClientRequest::InocodeKnowledgeWrite {
            request_id: RequestId::Integer(1),
            params: InocodeKnowledgeWriteParams {
                scope: InocodeKnowledgeScope::Project,
                cwd: cwd.clone(),
                record: input,
            },
        })
        .await
        .expect("write")
        .expect("payload");
        let json = serde_json::to_value(&w).unwrap();
        assert_eq!(json["id"], "rule-0001");
        let l = handle(ClientRequest::InocodeKnowledgeList {
            request_id: RequestId::Integer(2),
            params: InocodeKnowledgeListParams {
                scope: InocodeKnowledgeScope::Project,
                cwd,
                kind: None,
                status: None,
                query: Some("force".into()),
                limit: 10,
                cursor: None,
            },
        })
        .await
        .expect("list")
        .expect("payload");
        let json = serde_json::to_value(&l).unwrap();
        assert_eq!(json["records"][0]["id"], "rule-0001");
        assert_eq!(json["records"][0]["status"], "unverified");
    }
}
