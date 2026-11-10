//! `inocode/evidence/*`. Placeholder: empty-result responses (owner: cli-knowledge / evidence).

use super::InocodeResult;
use super::unknown_method;
use crate::error_code::invalid_params;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::InocodeEvidenceGraphResponse;
use codex_app_server_protocol::InocodeEvidenceListClaimsResponse;
use codex_app_server_protocol::InocodeEvidenceListSourcesResponse;
use codex_app_server_protocol::InocodeEvidenceVerifyDocumentResponse;

pub(crate) async fn handle(request: ClientRequest) -> InocodeResult {
    match request {
        ClientRequest::InocodeEvidenceListSources { .. } => Ok(Some(
            InocodeEvidenceListSourcesResponse {
                sources: Vec::new(),
                next_cursor: None,
            }
            .into(),
        )),
        ClientRequest::InocodeEvidenceReadSource { params, .. } => Err(invalid_params(format!(
            "evidence source not found: {}",
            params.id
        ))),
        ClientRequest::InocodeEvidenceListClaims { .. } => Ok(Some(
            InocodeEvidenceListClaimsResponse {
                claims: Vec::new(),
                next_cursor: None,
            }
            .into(),
        )),
        ClientRequest::InocodeEvidenceGraph { .. } => Ok(Some(
            InocodeEvidenceGraphResponse {
                nodes: Vec::new(),
                edges: Vec::new(),
            }
            .into(),
        )),
        ClientRequest::InocodeEvidenceVerifyDocument { .. } => Ok(Some(
            InocodeEvidenceVerifyDocumentResponse {
                findings: Vec::new(),
            }
            .into(),
        )),
        other => Err(unknown_method(other.method_name())),
    }
}
