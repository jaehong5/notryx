use super::{bearer, invalid_json, runtime_error, write};
use crate::CreateNode;
use crate::runtime::WorkspaceHandle;
use axum::Json;
use axum::extract::{State, rejection::JsonRejection};
use axum::http::HeaderMap;
use axum::response::Response;
use notryx_protocol::CreateNodeRequest;

pub(super) async fn create_node(
    State(handle): State<WorkspaceHandle>,
    headers: HeaderMap,
    body: Result<Json<CreateNodeRequest>, JsonRejection>,
) -> Response {
    let token = match bearer(&headers) {
        Ok(token) => token,
        Err(error) => return runtime_error(error),
    };
    let identity = match write::mutation_identity(&headers) {
        Ok(identity) => identity,
        Err(error) => return runtime_error(error),
    };
    let request = match body {
        Ok(Json(request)) => request,
        Err(error) => return invalid_json(error),
    };
    match handle
        .create_node(
            token,
            CreateNode {
                epoch: identity.epoch,
                stream_id: identity.stream_id,
                operation_seq: identity.operation_seq,
                operation_id: identity.operation_id,
                node_id: request.node_id,
                parent_id: request.parent_id,
                name: request.name,
                kind: request.kind,
                upload_id: request.upload_id,
            },
        )
        .await
        .and_then(write::receipt)
    {
        Ok(operation) => write::write_response(operation),
        Err(error) => runtime_error(error),
    }
}
