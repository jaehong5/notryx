use super::{api_error, bearer, invalid_json, runtime_error, write};
use crate::runtime::WorkspaceHandle;
use crate::{AbandonOperation, AckOperations};
use axum::Json;
use axum::extract::{State, rejection::JsonRejection};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use notryx_domain::ChangeSeq;
use notryx_protocol::{AbandonRequest, AckRequest, AckResponse};

pub(super) async fn ack(
    State(handle): State<WorkspaceHandle>,
    headers: HeaderMap,
    body: Result<Json<AckRequest>, JsonRejection>,
) -> Response {
    let token = match bearer(&headers) {
        Ok(token) => token,
        Err(error) => return runtime_error(error),
    };
    let request = match body {
        Ok(Json(request)) => request,
        Err(error) => return invalid_json(error),
    };
    let stream_id = match request.device_stream_id.parse() {
        Ok(id) => id,
        Err(_) => return api_error(StatusCode::BAD_REQUEST, "invalid_request"),
    };
    match handle
        .ack_operations(
            token,
            AckOperations {
                epoch: request.workspace_epoch,
                stream_id,
                ack_through: request.ack_through.get(),
            },
        )
        .await
    {
        Ok(state) => Json(AckResponse {
            workspace_epoch: state.workspace_epoch,
            device_stream_id: state.stream_id.to_string(),
            last_seq: ChangeSeq::new(state.last_seq),
            retired_through: ChangeSeq::new(state.retired_through),
        })
        .into_response(),
        Err(error) => runtime_error(error),
    }
}

pub(super) async fn abandon(
    State(handle): State<WorkspaceHandle>,
    headers: HeaderMap,
    body: Result<Json<AbandonRequest>, JsonRejection>,
) -> Response {
    let token = match bearer(&headers) {
        Ok(token) => token,
        Err(error) => return runtime_error(error),
    };
    let request = match body {
        Ok(Json(request)) => request,
        Err(error) => return invalid_json(error),
    };
    let stream_id = match request.device_stream_id.parse() {
        Ok(id) => id,
        Err(_) => return api_error(StatusCode::BAD_REQUEST, "invalid_request"),
    };
    match handle
        .abandon_operation(
            token,
            AbandonOperation {
                epoch: request.workspace_epoch,
                stream_id,
                operation_id: request.operation_id,
                operation_seq: request.operation_seq.get(),
            },
        )
        .await
        .and_then(write::receipt)
    {
        Ok(operation) => write::write_response(operation),
        Err(error) => runtime_error(error),
    }
}
