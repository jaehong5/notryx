use super::{api_error, bearer, node_id, runtime_error};
use crate::auth::AuthError;
use crate::runtime::{RuntimeError, WorkspaceHandle};
use axum::{
    Json,
    extract::{Path, RawQuery, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use notryx_domain::{ChangeSeq, NodeId};

fn changes_query(query: Option<&str>) -> Result<(ChangeSeq, Option<ChangeSeq>), RuntimeError> {
    let invalid = RuntimeError::Auth(AuthError::InvalidRequest);
    let mut after = None;
    let mut through = None;
    for (name, value) in form_urlencoded::parse(query.unwrap_or_default().as_bytes()) {
        match name.as_ref() {
            "after" if after.is_none() => {
                after = Some(ChangeSeq::try_from(value.into_owned()).map_err(|_| invalid)?)
            }
            "through" if through.is_none() => {
                through = Some(ChangeSeq::try_from(value.into_owned()).map_err(|_| invalid)?)
            }
            _ => return Err(invalid),
        }
    }
    Ok((after.ok_or(invalid)?, through))
}

pub(super) async fn changes(
    State(handle): State<WorkspaceHandle>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    let token = match bearer(&headers) {
        Ok(token) => token,
        Err(error) => return runtime_error(error),
    };
    let (after, through) = match changes_query(query.as_deref()) {
        Ok(query) => query,
        Err(error) => return runtime_error(error),
    };
    match handle.changes(token, after, through).await {
        Ok(page) => Json(page).into_response(),
        Err(error) => runtime_error(error),
    }
}

fn inventory_query(query: Option<&str>) -> Result<(Option<NodeId>, u16), RuntimeError> {
    let invalid = RuntimeError::Auth(AuthError::InvalidRequest);
    let mut after = None;
    let mut limit = None;
    for (name, value) in form_urlencoded::parse(query.unwrap_or_default().as_bytes()) {
        match name.as_ref() {
            "after_id" if after.is_none() => {
                after = Some(value.parse::<NodeId>().map_err(|_| invalid)?);
            }
            "limit" if limit.is_none() => {
                if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                    return Err(invalid);
                }
                let count = value.parse::<u16>().map_err(|_| invalid)?;
                if !(1..=100).contains(&count) {
                    return Err(invalid);
                }
                limit = Some(count);
            }
            _ => return Err(invalid),
        }
    }
    Ok((after, limit.unwrap_or(100)))
}

pub(super) async fn trash(
    State(handle): State<WorkspaceHandle>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    let token = match bearer(&headers) {
        Ok(token) => token,
        Err(error) => return runtime_error(error),
    };
    let (after_id, limit) = match inventory_query(query.as_deref()) {
        Ok(query) => query,
        Err(error) => return runtime_error(error),
    };
    match handle.trash(token, after_id, limit).await {
        Ok(page) => Json(page).into_response(),
        Err(error) => runtime_error(error),
    }
}

pub(super) async fn inventory(
    State(handle): State<WorkspaceHandle>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    let token = match bearer(&headers) {
        Ok(token) => token,
        Err(error) => return runtime_error(error),
    };
    let (after_id, limit) = match inventory_query(query.as_deref()) {
        Ok(query) => query,
        Err(error) => return runtime_error(error),
    };
    match handle.inventory(token, after_id, limit).await {
        Ok(page) => Json(page).into_response(),
        Err(error) => runtime_error(error),
    }
}

pub(super) async fn node(
    State(handle): State<WorkspaceHandle>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let token = match bearer(&headers) {
        Ok(token) => token,
        Err(error) => return runtime_error(error),
    };
    let id = match node_id(&id) {
        Ok(id) => id,
        Err(error) => return runtime_error(error),
    };
    match handle.node_state(token, id).await {
        Ok(node) => {
            let etag = match HeaderValue::from_str(&node.node_etag) {
                Ok(etag) => etag,
                Err(_) => return api_error(StatusCode::SERVICE_UNAVAILABLE, "storage_unavailable"),
            };
            let mut response = Json(node).into_response();
            response.headers_mut().insert(header::ETAG, etag);
            response
        }
        Err(error) => runtime_error(error),
    }
}
