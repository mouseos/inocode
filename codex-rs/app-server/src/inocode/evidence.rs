//! `inocode/evidence/*`: backed by the `codex-inocode-evidence` crate
//! (`<INOCODE_HOME>/evidence/`; originals are stored unchanged).

use super::InocodeResult;
use super::unknown_method;
use crate::error_code::internal_error;
use crate::error_code::invalid_params;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::InocodeEvidenceAddClaimParams;
use codex_app_server_protocol::InocodeEvidenceAddClaimResponse;
use codex_app_server_protocol::InocodeEvidenceAddQuoteParams;
use codex_app_server_protocol::InocodeEvidenceAddQuoteResponse;
use codex_app_server_protocol::InocodeEvidenceFinding;
use codex_app_server_protocol::InocodeEvidenceGraphResponse;
use codex_app_server_protocol::InocodeEvidenceImportSourceParams;
use codex_app_server_protocol::InocodeEvidenceImportSourceResponse;
use codex_app_server_protocol::InocodeEvidenceListClaimsResponse;
use codex_app_server_protocol::InocodeEvidenceListSourcesResponse;
use codex_app_server_protocol::InocodeEvidenceListSpansResponse;
use codex_app_server_protocol::InocodeEvidenceReadSourceResponse;
use codex_app_server_protocol::InocodeEvidenceRemoveQuoteResponse;
use codex_app_server_protocol::InocodeEvidenceSpan;
use codex_app_server_protocol::InocodeEvidenceUpdateSourceResponse;
use codex_app_server_protocol::InocodeEvidenceSetClaimStatusResponse;
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
            let titles: std::collections::HashMap<String, String> = store
                .list_sources()
                .into_iter()
                .map(|s| (s.id, s.title))
                .collect();
            let spans: std::collections::HashMap<String, ev::Span> = store
                .list_spans()
                .into_iter()
                .map(|s| (s.id.clone(), s))
                .collect();
            let claims = claims
                .iter()
                .map(|c| {
                    let evidence: Vec<Value> = c
                        .span_ids
                        .iter()
                        .filter_map(|id| spans.get(id))
                        .map(|s| {
                            serde_json::json!({
                                "spanId": s.id,
                                "sourceTitle": titles.get(&s.source_id).cloned().unwrap_or_default(),
                                "quote": s.text,
                            })
                        })
                        .collect();
                    convert(c, |m| {
                        m.entry("confidence")
                            .or_insert(Value::String("medium".into()));
                        m.insert("evidence".into(), Value::Array(evidence));
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
        ClientRequest::InocodeEvidenceListSpans { params, .. } => {
            let spans = store
                .list_spans()
                .into_iter()
                .filter(|s| params.source_id.as_deref().is_none_or(|id| s.source_id == id))
                .map(|s| InocodeEvidenceSpan {
                    id: s.id,
                    source_id: s.source_id,
                    quote: s.text,
                    start: s.start as u32,
                    end: s.end as u32,
                })
                .collect();
            Ok(Some(InocodeEvidenceListSpansResponse { spans }.into()))
        }
        ClientRequest::InocodeEvidenceRemoveQuote { params, .. } => {
            store
                .remove_span(&params.span_id)
                .map_err(|e| invalid_params(format!("evidence removeQuote failed: {e}")))?;
            Ok(Some(InocodeEvidenceRemoveQuoteResponse {}.into()))
        }
        ClientRequest::InocodeEvidenceUpdateSource { params, .. } => {
            let src = store
                .update_source(
                    &params.id,
                    params.title.as_deref(),
                    params.kind.as_deref(),
                    params.version.as_deref(),
                )
                .map_err(|e| invalid_params(format!("evidence updateSource failed: {e}")))?;
            Ok(Some(
                InocodeEvidenceUpdateSourceResponse {
                    source: convert(&src, |_| {})?,
                }
                .into(),
            ))
        }
        ClientRequest::InocodeEvidenceImportSource { params, .. } => {
            Ok(Some(import_source(&store, params)?.into()))
        }
        ClientRequest::InocodeEvidenceAddQuote { params, .. } => {
            Ok(Some(add_quote(&store, params)?.into()))
        }
        ClientRequest::InocodeEvidenceAddClaim { params, .. } => {
            Ok(Some(add_claim(&store, params)?.into()))
        }
        ClientRequest::InocodeEvidenceSetClaimStatus { params, .. } => {
            store
                .set_claim_status(&params.id, &params.status)
                .map_err(|e| invalid_params(format!("evidence setClaimStatus failed: {e}")))?;
            Ok(Some(InocodeEvidenceSetClaimStatusResponse {}.into()))
        }
        other => Err(unknown_method(other.method_name())),
    }
}

fn import_source(
    store: &ev::Store,
    p: InocodeEvidenceImportSourceParams,
) -> Result<InocodeEvidenceImportSourceResponse, JSONRPCErrorError> {
    let version = p.version.as_deref();
    let explicit = p.title.as_deref().filter(|t| !t.trim().is_empty());
    let src = match (p.text.as_deref(), p.path.as_deref()) {
        (Some(text), _) => {
            let title = explicit
                .map(str::to_string)
                .or_else(|| p.file_name.clone().filter(|n| !n.trim().is_empty()))
                .unwrap_or_else(|| {
                    text.lines()
                        .find(|l| !l.trim().is_empty())
                        .unwrap_or("")
                        .trim()
                        .chars()
                        .take(40)
                        .collect()
                });
            store.import_text(&title, &p.kind, text, version)
        }
        (None, Some(path)) => store.import_file(
            std::path::Path::new(path),
            explicit.or(p.file_name.as_deref().filter(|n| !n.trim().is_empty())),
            &p.kind,
            version,
        ),
        (None, None) => {
            return Err(invalid_params("importSource needs `text` or `path`".to_string()));
        }
    }
    .map_err(|e| invalid_params(format!("evidence importSource failed: {e}")))?;
    Ok(InocodeEvidenceImportSourceResponse {
        id: src.id,
        hash: src.hash,
    })
}

fn add_quote(
    store: &ev::Store,
    p: InocodeEvidenceAddQuoteParams,
) -> Result<InocodeEvidenceAddQuoteResponse, JSONRPCErrorError> {
    let span = match (p.quote.as_deref(), p.start, p.end) {
        (quote, Some(start), Some(end)) => store
            .add_span(&p.source_id, start as usize, end as usize)
            .and_then(|span| match quote {
                Some(q) if !q.is_empty() && q != span.text => {
                    store.remove_span(&span.id)?;
                    Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "quote does not match the text at start..end",
                    ))
                }
                _ => Ok(span),
            }),
        (Some(quote), _, _) => store.add_quote(&p.source_id, quote),
        _ => {
            return Err(invalid_params(
                "addQuote needs `quote`, or `start` and `end`".to_string(),
            ));
        }
    }
    .map_err(|e| invalid_params(format!("evidence addQuote failed: {e}")))?;
    Ok(InocodeEvidenceAddQuoteResponse { span_id: span.id })
}

fn add_claim(
    store: &ev::Store,
    p: InocodeEvidenceAddClaimParams,
) -> Result<InocodeEvidenceAddClaimResponse, JSONRPCErrorError> {
    let claim_type = ev::ClaimType::parse(&p.type_)
        .ok_or_else(|| invalid_params(format!("unknown claim type: {}", p.type_)))?;
    let confidence: ev::Confidence = convert(&p.confidence, |_| {})?;
    let claim = store
        .add_claim(ev::NewClaim {
            text: &p.text,
            claim_type,
            status: "unverified",
            confidence: Some(confidence),
            span_ids: p.span_ids,
            premise_ids: p.premise_ids,
            created_by: &p.created_by,
        })
        .map_err(|e| invalid_params(format!("evidence addClaim failed: {e}")))?;
    Ok(InocodeEvidenceAddClaimResponse { id: claim.id })
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_app_server_protocol::InocodeKnowledgeConfidence;

    #[test]
    fn write_chain_source_quote_claim_status() {
        let dir = tempfile::tempdir().unwrap();
        let store = ev::Store::new(dir.path().to_path_buf());
        let src = import_source(
            &store,
            InocodeEvidenceImportSourceParams {
                title: Some("法令".into()),
                file_name: None,
                kind: "text".into(),
                text: Some("第一条 目的は安全である。".into()),
                path: None,
                version: None,
            },
        )
        .unwrap();
        assert!(!src.hash.is_empty());
        let span = add_quote(
            &store,
            InocodeEvidenceAddQuoteParams {
                source_id: src.id.clone(),
                quote: Some("目的は安全である".into()),
                start: None,
                end: None,
            },
        )
        .unwrap();
        let claim = add_claim(
            &store,
            InocodeEvidenceAddClaimParams {
                text: "目的は安全".into(),
                type_: "fact".into(),
                confidence: InocodeKnowledgeConfidence::High,
                span_ids: vec![span.span_id.clone()],
                premise_ids: vec![],
                created_by: "user".into(),
            },
        )
        .unwrap();
        store.set_claim_status(&claim.id, "verified").unwrap();
        let claims = store.list_claims(Some("verified"));
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].span_ids, vec![span.span_id]);
        assert!(store.claim_support()[&claim.id]);
        assert!(
            add_claim(
                &store,
                InocodeEvidenceAddClaimParams {
                    text: "x".into(),
                    type_: "bogus".into(),
                    confidence: InocodeKnowledgeConfidence::Low,
                    span_ids: vec![],
                    premise_ids: vec![],
                    created_by: "user".into(),
                },
            )
            .is_err()
        );
    }
}
