//! `inocode/*` extension methods (experimental). Contract: ref/docs/09-overnight-contract.md ch.4.
//!
//! Each area lives in its own file and exposes `handle(ClientRequest)`.
//! `flags` is implemented; the others return fixture-shaped placeholder
//! responses until their owners replace them.

mod evidence;
mod flags;
mod knowledge;
mod metrics;
mod team;

use crate::error_code::method_not_found;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::ClientResponsePayload;
use codex_app_server_protocol::JSONRPCErrorError;

pub(crate) type InocodeResult = Result<Option<ClientResponsePayload>, JSONRPCErrorError>;

/// Routes an `inocode/*` request to its area handler. Any other method gets
/// the same "method not found" error as an unknown official method.
pub(crate) async fn handle(request: ClientRequest) -> InocodeResult {
    let method = request.method_name();
    match method.strip_prefix("inocode/").and_then(|m| m.split('/').next()) {
        Some("knowledge") => knowledge::handle(request).await,
        Some("evidence") => evidence::handle(request).await,
        Some("team") => team::handle(request).await,
        Some("metrics") => metrics::handle(request).await,
        Some("flags") => flags::handle(request).await,
        _ => Err(unknown_method(method)),
    }
}

pub(crate) fn unknown_method(method: &str) -> JSONRPCErrorError {
    method_not_found(format!("Method not found: {method}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_app_server_protocol::InocodeMetricsSummaryParams;
    use codex_app_server_protocol::RequestId;

    #[tokio::test]
    async fn every_area_answers() {
        let req = ClientRequest::InocodeMetricsSummary {
            request_id: RequestId::Integer(1),
            params: InocodeMetricsSummaryParams {
                since: "2026-11-11T00:00:00Z".to_string(),
            },
        };
        assert!(handle(req).await.expect("ok").is_some());
    }
}
