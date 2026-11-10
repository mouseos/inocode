//! `inocode/evidence/*`: backed by the `codex-inocode-evidence` crate
//! (`<INOCODE_HOME>/evidence/`; originals are stored unchanged).

use super::InocodeResult;
use super::unknown_method;
use crate::error_code::internal_error;
use crate::error_code::invalid_params;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::InocodeEvidenceFinding;
use codex_app_server_protocol::InocodeEvidenceGraphResponse;
use codex_app_server_protocol::InocodeEvidenceListClaimsResponse;
use codex_app_server_protocol::InocodeEvidenceListSourcesResponse;
use codex_app_server_protocol::InocodeEvidenceReadSourceResponse;
use codex_app_server_protocol::InocodeEvidenceTextPosition;
use codex_app_server_protocol::InocodeEvidenceVerifyDocumentResponse;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_inocode_evidence as ev;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

const DEFAULT_LIMIT: usize = 100;

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

fn page<T>(items: Vec<T>, limit: u32, cursor: Option<String>) -> (Vec<T>, Option<String>) {
    let offset = cursor.and_then(|c| c.parse::<usize>().ok()).unwrap_or(0);
    let limit = if limit == 0 {
        DEFAULT_LIMIT
    } else {
        limit as usize
    };
    let total = items.len();
    let out: Vec<T> = items.into_iter().skip(offset).take(limit).collect();
    let next = (offset + out.len() < total).then(|| (offset + out.len()).to_string());
    (out, next)
}

pub(crate) async fn handle(request: ClientRequest) -> InocodeResult {
    let store = ev::default_store();
    match request {
        ClientRequest::InocodeEvidenceListSources { params, .. } => {
            let (sources, next_cursor) = page(store.list_sources(), params.limit, params.cursor);
            let sources = sources
                .iter()
                .map(|s| convert(s, |_| {}))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Some(
                InocodeEvidenceListSourcesResponse {
                    sources,
                    next_cursor,
                }
                .into(),
            ))
        }
        ClientRequest::InocodeEvidenceReadSource { params, .. } => {
            let (src, text) = store.read_source(&params.id).ok_or_else(|| {
                invalid_params(format!("evidence source not found: {}", params.id))
            })?;
            Ok(Some(
                InocodeEvidenceReadSourceResponse {
                    source: convert(&src, |_| {})?,
                    text,
                }
                .into(),
            ))
        }
        ClientRequest::InocodeEvidenceListClaims { params, .. } => {
            let claims = store.list_claims(params.status.as_deref());
            let (claims, next_cursor) = page(claims, params.limit, params.cursor);
            let claims = claims
                .iter()
                .map(|c| {
                    convert(c, |m| {
                        m.entry("confidence")
                            .or_insert(Value::String("medium".into()));
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Some(
                InocodeEvidenceListClaimsResponse {
                    claims,
                    next_cursor,
                }
                .into(),
            ))
        }
        ClientRequest::InocodeEvidenceGraph { params, .. } => {
            let g = store.graph(params.root_claim_id.as_deref());
            Ok(Some(
                InocodeEvidenceGraphResponse {
                    nodes: g
                        .nodes
                        .iter()
                        .map(|n| convert(n, |_| {}))
                        .collect::<Result<_, _>>()?,
                    edges: g
                        .edges
                        .iter()
                        .map(|e| convert(e, |_| {}))
                        .collect::<Result<_, _>>()?,
                }
                .into(),
            ))
        }
        ClientRequest::InocodeEvidenceVerifyDocument { params, .. } => {
            let ids: Vec<String> = if params.source_ids.is_empty() {
                store.list_sources().into_iter().map(|s| s.id).collect()
            } else {
                params.source_ids
            };
            let mut texts = Vec::new();
            for id in &ids {
                let (_, t) = store.read_source(id).ok_or_else(|| {
                    invalid_params(format!("evidence source not found: {id}"))
                })?;
                texts.push(t);
            }
            let findings = ev::verify_document(&params.text, &texts)
                .into_iter()
                .map(|f| {
                    let start = f.position as u32;
                    let end = start + f.text.chars().count() as u32;
                    Ok(InocodeEvidenceFinding {
                        kind: convert(&f.kind, |_| {})?,
                        text: f.text,
                        position: InocodeEvidenceTextPosition { start, end },
                    })
                })
                .collect::<Result<Vec<_>, JSONRPCErrorError>>()?;
            Ok(Some(
                InocodeEvidenceVerifyDocumentResponse { findings }.into(),
            ))
        }
        other => Err(unknown_method(other.method_name())),
    }
}
