use super::{api_error, bearer, invalid_json, node_id, runtime_error, write};
use crate::runtime::WorkspaceHandle;
use crate::{DeleteNode, MoveNode, RestoreNode};
use axum::Json;
use axum::body::Bytes;
use axum::extract::{FromRequest, Path, Request, State, rejection::JsonRejection};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use notryx_protocol::{MetadataPrecondition, MoveNodeRequest, RestoreNodeRequest, StrongEtag};

pub(super) async fn delete_node(
    State(handle): State<WorkspaceHandle>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    let headers = request.headers();
    let token = match bearer(headers) {
        Ok(token) => token,
        Err(error) => return runtime_error(error),
    };
    let node_id = match node_id(&id) {
        Ok(id) => id,
        Err(error) => return runtime_error(error),
    };
    let identity = match write::mutation_identity(headers) {
        Ok(identity) => identity,
        Err(error) => return runtime_error(error),
    };
    if !headers.contains_key(header::IF_MATCH) {
        return api_error(StatusCode::PRECONDITION_REQUIRED, "invalid_request");
    }
    let expected_node_etag = match write::required_header(headers, "if-match")
        .ok()
        .and_then(|value| StrongEtag::parse(value).ok())
    {
        Some(etag) => etag.as_str().to_owned(),
        None => return api_error(StatusCode::BAD_REQUEST, "invalid_request"),
    };
    let body = match Bytes::from_request(request, &handle).await {
        Ok(body) => body,
        Err(error) => return api_error(error.status(), "invalid_request"),
    };
    if !body.is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "invalid_request");
    }
    match handle
        .delete_node(
            token,
            DeleteNode {
                epoch: identity.epoch,
                stream_id: identity.stream_id,
                operation_seq: identity.operation_seq,
                operation_id: identity.operation_id,
                node_id,
                expected_node_etag,
            },
        )
        .await
        .and_then(write::receipt)
    {
        Ok(operation) => write::write_response(operation),
        Err(error) => runtime_error(error),
    }
}

pub(super) async fn restore_node(
    State(handle): State<WorkspaceHandle>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Result<Json<RestoreNodeRequest>, JsonRejection>,
) -> Response {
    let token = match bearer(&headers) {
        Ok(token) => token,
        Err(error) => return runtime_error(error),
    };
    let node_id = match node_id(&id) {
        Ok(id) => id,
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
        .restore_node(
            token,
            RestoreNode {
                epoch: identity.epoch,
                stream_id: identity.stream_id,
                operation_seq: identity.operation_seq,
                operation_id: identity.operation_id,
                node_id,
                expected_meta_rev: request.expected_meta_rev,
                parent_id: request.parent_id,
                name: request.name,
            },
        )
        .await
        .and_then(write::receipt)
    {
        Ok(operation) => write::write_response(operation),
        Err(error) => runtime_error(error),
    }
}

pub(super) async fn move_node(
    State(handle): State<WorkspaceHandle>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Result<Json<MoveNodeRequest>, JsonRejection>,
) -> Response {
    let token = match bearer(&headers) {
        Ok(token) => token,
        Err(error) => return runtime_error(error),
    };
    let node_id = match node_id(&id) {
        Ok(id) => id,
        Err(error) => return runtime_error(error),
    };
    let identity = match write::mutation_identity(&headers) {
        Ok(identity) => identity,
        Err(error) => return runtime_error(error),
    };
    if !headers.contains_key(header::IF_MATCH) {
        return api_error(StatusCode::PRECONDITION_REQUIRED, "invalid_request");
    }
    let expected_meta_rev = match write::required_header(&headers, "if-match")
        .ok()
        .and_then(|value| MetadataPrecondition::parse(value).ok())
        .and_then(|condition| condition.revision().ok())
    {
        Some(revision) => revision,
        None => return api_error(StatusCode::BAD_REQUEST, "invalid_request"),
    };
    let request = match body {
        Ok(Json(request)) => request,
        Err(error) => return invalid_json(error),
    };
    match handle
        .move_node(
            token,
            MoveNode {
                epoch: identity.epoch,
                stream_id: identity.stream_id,
                operation_seq: identity.operation_seq,
                operation_id: identity.operation_id,
                node_id,
                expected_meta_rev,
                expected_parent_id: request.expected_parent_id,
                parent_id: request.parent_id,
                name: request.name,
            },
        )
        .await
        .and_then(write::receipt)
    {
        Ok(operation) => write::write_response(operation),
        Err(error) => runtime_error(error),
    }
}
