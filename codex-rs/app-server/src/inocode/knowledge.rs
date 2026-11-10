//! `inocode/knowledge/*`. Placeholder: empty-result responses (owner: cli-knowledge).

use super::InocodeResult;
use super::unknown_method;
use crate::error_code::invalid_params;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::InocodeKnowledgeListResponse;
use codex_app_server_protocol::InocodeKnowledgeSearchResponse;
use codex_app_server_protocol::InocodeKnowledgeSetStatusResponse;
use codex_app_server_protocol::InocodeKnowledgeWriteResponse;

pub(crate) async fn handle(request: ClientRequest) -> InocodeResult {
    match request {
        ClientRequest::InocodeKnowledgeList { .. } => Ok(Some(
            InocodeKnowledgeListResponse {
                records: Vec::new(),
                next_cursor: None,
            }
            .into(),
        )),
        ClientRequest::InocodeKnowledgeRead { params, .. } => Err(invalid_params(format!(
            "knowledge record not found: {}",
            params.id
        ))),
        ClientRequest::InocodeKnowledgeWrite { params, .. } => Ok(Some(
            InocodeKnowledgeWriteResponse {
                id: params.record.id.unwrap_or_default(),
            }
            .into(),
        )),
        ClientRequest::InocodeKnowledgeSetStatus { .. } => {
            Ok(Some(InocodeKnowledgeSetStatusResponse {}.into()))
        }
        ClientRequest::InocodeKnowledgeSearch { .. } => Ok(Some(
            InocodeKnowledgeSearchResponse {
                records: Vec::new(),
                next_cursor: None,
            }
            .into(),
        )),
        other => Err(unknown_method(other.method_name())),
    }
}
