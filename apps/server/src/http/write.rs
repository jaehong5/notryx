use super::{api_error, bearer, content_precondition, error_body, node_id, runtime_error};
use crate::auth::AuthError;
use crate::runtime::{RuntimeError, WorkspaceHandle};
use crate::{OperationRecord, Outcome, SaveContent};
use axum::Json;
use axum::body::Bytes;
use axum::extract::{FromRequest, Path, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use notryx_domain::{ChangeSeq, ContentRevision, NodeId, OperationId, WorkspaceEpoch};
use notryx_protocol::{OperationError, OperationReceipt, StrongEtag, verify_content_digest};
use uuid::{Uuid, Variant};

pub(super) fn required_header<'a>(
    headers: &'a HeaderMap,
    name: &str,
) -> Result<&'a str, RuntimeError> {
    let invalid = RuntimeError::Auth(AuthError::InvalidRequest);
    let mut values = headers.get_all(name).iter();
    let value = values.next().ok_or(invalid)?;
    if values.next().is_some() {
        return Err(invalid);
    }
    value.to_str().map_err(|_| invalid)
}

pub(super) struct MutationIdentity {
    pub epoch: WorkspaceEpoch,
    pub stream_id: Uuid,
    pub operation_seq: u64,
    pub operation_id: OperationId,
}

pub(super) fn mutation_identity(headers: &HeaderMap) -> Result<MutationIdentity, RuntimeError> {
    let invalid = RuntimeError::Auth(AuthError::InvalidRequest);
    if headers.contains_key(header::CONTENT_ENCODING)
        && !required_header(headers, "content-encoding")?.eq_ignore_ascii_case("identity")
    {
        return Err(invalid);
    }
    let stream_id: Uuid = required_header(headers, "x-device-stream")?
        .parse()
        .map_err(|_| invalid)?;
    if stream_id.get_version_num() != 4 || stream_id.get_variant() != Variant::RFC4122 {
        return Err(invalid);
    }
    let sequence = required_header(headers, "x-operation-seq")?;
    if sequence.is_empty() || !sequence.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid);
    }
    let operation_seq = sequence.parse::<u64>().map_err(|_| invalid)?;
    if operation_seq == 0 {
        return Err(invalid);
    }
    Ok(MutationIdentity {
        epoch: required_header(headers, "x-workspace-epoch")?
            .parse()
            .map_err(|_| invalid)?,
        stream_id,
        operation_seq,
        operation_id: required_header(headers, "idempotency-key")?
            .parse()
            .map_err(|_| invalid)?,
    })
}

fn mutation(
    headers: &HeaderMap,
    node_id: NodeId,
    expected_content_rev: ContentRevision,
) -> Result<(SaveContent, String), RuntimeError> {
    let identity = mutation_identity(headers)?;
    Ok((
        SaveContent {
            epoch: identity.epoch,
            stream_id: identity.stream_id,
            operation_seq: identity.operation_seq,
            operation_id: identity.operation_id,
            node_id,
            expected_content_rev,
            content_hash: String::new(),
            bytes: Vec::new(),
        },
        required_header(headers, "content-digest")?.into(),
    ))
}

pub(super) async fn put_content(
    State(handle): State<WorkspaceHandle>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    let headers = request.headers();
    let token = match bearer(headers) {
        Ok(token) => token,
        Err(error) => return runtime_error(error),
    };
    let id = match node_id(&id) {
        Ok(id) => id,
        Err(error) => return runtime_error(error),
    };
    let precondition = match content_precondition(headers) {
        Ok(Some(value)) => match value.revision() {
            Ok(revision) => revision,
            Err(_) => return api_error(StatusCode::BAD_REQUEST, "invalid_request"),
        },
        Ok(None) => return api_error(StatusCode::PRECONDITION_REQUIRED, "invalid_request"),
        Err(error) => return runtime_error(error),
    };
    let (mut save, digest) = match mutation(headers, id, precondition) {
        Ok(value) => value,
        Err(error) => return runtime_error(error),
    };
    let Some(permit) = request
        .extensions()
        .get::<std::sync::Arc<tokio::sync::OwnedSemaphorePermit>>()
        .cloned()
    else {
        return runtime_error(RuntimeError::Worker);
    };
    let bytes = match Bytes::from_request(request, &handle).await {
        Ok(bytes) => bytes,
        Err(error) => return api_error(error.status(), "invalid_request"),
    };
    if verify_content_digest(&digest, &bytes).is_err() {
        return api_error(StatusCode::BAD_REQUEST, "digest_mismatch");
    }
    save.content_hash = crate::hash(&bytes);
    save.bytes = bytes.to_vec();
    drop(bytes);
    match handle
        .save_content_bounded(token, save, permit)
        .await
        .and_then(receipt)
    {
        Ok(operation) => write_response(operation),
        Err(error) => runtime_error(error),
    }
}

pub(super) async fn operation(
    State(handle): State<WorkspaceHandle>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let token = match bearer(&headers) {
        Ok(token) => token,
        Err(error) => return runtime_error(error),
    };
    let id: OperationId = match id.parse() {
        Ok(id) => id,
        Err(_) => return api_error(StatusCode::BAD_REQUEST, "invalid_request"),
    };
    match handle.operation(token, id).await.and_then(receipt) {
        Ok(operation) => Json(operation).into_response(),
        Err(error) => runtime_error(error),
    }
}

pub(super) fn receipt(record: OperationRecord) -> Result<OperationReceipt, RuntimeError> {
    let invalid = RuntimeError::Workspace(crate::Error::InvalidFormat);
    let operation_id = record.operation_id;
    let operation_seq = ChangeSeq::new(record.operation_seq);
    let workspace_epoch = record.workspace_epoch;
    Ok(match record.outcome {
        Outcome::RecoveryConflict => OperationReceipt::RecoveryConflict {
            operation_id,
            operation_seq,
            workspace_epoch,
        },
        Outcome::Deleted(snapshot) => {
            let mut node = snapshot.representation().map_err(|_| invalid)?;
            node.node_etag = record.node_etag.ok_or(invalid)?;
            OperationReceipt::Deleted {
                operation_id,
                operation_seq,
                workspace_epoch,
                commit_seq: snapshot.last_change_seq,
                node,
            }
        }
        Outcome::Restored(snapshot) => {
            let mut node = snapshot.representation().map_err(|_| invalid)?;
            node.node_etag = record.node_etag.ok_or(invalid)?;
            OperationReceipt::Restored {
                operation_id,
                operation_seq,
                workspace_epoch,
                commit_seq: snapshot.last_change_seq,
                node,
            }
        }
        Outcome::NodePreconditionFailed { current } => OperationReceipt::NodePreconditionFailed {
            operation_id,
            operation_seq,
            workspace_epoch,
            current_node_etag: current,
        },
        Outcome::NodeDeleted => OperationReceipt::NodeDeleted {
            operation_id,
            operation_seq,
            workspace_epoch,
        },
        Outcome::NotDeleted => OperationReceipt::NotDeleted {
            operation_id,
            operation_seq,
            workspace_epoch,
        },
        Outcome::FolderNotEmpty => OperationReceipt::FolderNotEmpty {
            operation_id,
            operation_seq,
            workspace_epoch,
        },
        Outcome::HistoryExpired => OperationReceipt::HistoryExpired {
            operation_id,
            operation_seq,
            workspace_epoch,
        },
        Outcome::Moved(snapshot) => {
            let mut node = snapshot.representation().map_err(|_| invalid)?;
            node.node_etag = record.node_etag.ok_or(invalid)?;
            OperationReceipt::Moved {
                operation_id,
                operation_seq,
                workspace_epoch,
                commit_seq: snapshot.last_change_seq,
                node,
            }
        }
        Outcome::MetadataPreconditionFailed { current } => {
            OperationReceipt::MetadataPreconditionFailed {
                operation_id,
                operation_seq,
                workspace_epoch,
                current_meta_etag: StrongEtag::from_meta_revision(&current)
                    .map_err(|_| invalid)?
                    .as_str()
                    .into(),
                current_meta_rev: current,
            }
        }
        Outcome::ExpectedParentMismatch => OperationReceipt::ExpectedParentMismatch {
            operation_id,
            operation_seq,
            workspace_epoch,
        },
        Outcome::CycleDetected => OperationReceipt::CycleDetected {
            operation_id,
            operation_seq,
            workspace_epoch,
        },
        Outcome::RootImmutable => OperationReceipt::RootImmutable {
            operation_id,
            operation_seq,
            workspace_epoch,
        },
        Outcome::Created(snapshot) => {
            let mut node = snapshot.representation().map_err(|_| invalid)?;
            node.node_etag = record.node_etag.ok_or(invalid)?;
            OperationReceipt::Created {
                operation_id,
                operation_seq,
                workspace_epoch,
                commit_seq: snapshot.last_change_seq,
                node,
            }
        }
        Outcome::DestinationExists => OperationReceipt::DestinationExists {
            operation_id,
            operation_seq,
            workspace_epoch,
        },
        Outcome::NodeIdConflict => OperationReceipt::NodeIdConflict {
            operation_id,
            operation_seq,
            workspace_epoch,
        },
        Outcome::ParentNotFolder => OperationReceipt::ParentNotFolder {
            operation_id,
            operation_seq,
            workspace_epoch,
        },
        Outcome::Saved(document) => {
            let content_etag =
                StrongEtag::from_content_revision(&document.content_rev).map_err(|_| invalid)?;
            let meta_etag =
                StrongEtag::from_meta_revision(&document.meta_rev).map_err(|_| invalid)?;
            OperationReceipt::Committed {
                operation_id,
                operation_seq,
                workspace_epoch,
                node_id: document.node_id,
                content_rev: document.content_rev,
                content_etag: content_etag.as_str().into(),
                meta_rev: document.meta_rev,
                meta_etag: meta_etag.as_str().into(),
                node_etag: record.node_etag.ok_or(invalid)?,
                content_hash: format!("sha256:{}", document.content_hash),
                commit_seq: document.last_change_seq,
            }
        }
        Outcome::PreconditionFailed { current } => OperationReceipt::PreconditionFailed {
            operation_id,
            operation_seq,
            workspace_epoch,
            current_content_etag: StrongEtag::from_content_revision(&current)
                .map_err(|_| invalid)?
                .as_str()
                .into(),
            current_content_rev: current,
        },
        Outcome::NotFound => OperationReceipt::NotFound {
            operation_id,
            operation_seq,
            workspace_epoch,
        },
        Outcome::NotApplied => OperationReceipt::NotApplied {
            operation_id,
            operation_seq,
            workspace_epoch,
        },
        Outcome::Abandoned => OperationReceipt::Abandoned {
            operation_id,
            operation_seq,
            workspace_epoch,
        },
        Outcome::Pending => OperationReceipt::Pending {
            operation_id,
            operation_seq,
            workspace_epoch,
        },
    })
}

pub(super) fn write_response(operation: OperationReceipt) -> Response {
    let (status, code) = match &operation {
        OperationReceipt::Deleted { .. }
        | OperationReceipt::Restored { .. }
        | OperationReceipt::Moved { .. }
        | OperationReceipt::Created { .. }
        | OperationReceipt::Committed { .. }
        | OperationReceipt::Abandoned { .. } => {
            return Json(operation).into_response();
        }
        OperationReceipt::NodePreconditionFailed { .. } => {
            (StatusCode::PRECONDITION_FAILED, "node_revision_mismatch")
        }
        OperationReceipt::NodeDeleted { .. } => (StatusCode::GONE, "node_deleted"),
        OperationReceipt::HistoryExpired { .. } => (StatusCode::GONE, "history_expired"),
        OperationReceipt::NotDeleted { .. } => (StatusCode::CONFLICT, "not_deleted"),
        OperationReceipt::FolderNotEmpty { .. } => (StatusCode::CONFLICT, "folder_not_empty"),
        OperationReceipt::PreconditionFailed { .. } => {
            (StatusCode::PRECONDITION_FAILED, "content_revision_mismatch")
        }
        OperationReceipt::MetadataPreconditionFailed { .. } => (
            StatusCode::PRECONDITION_FAILED,
            "metadata_revision_mismatch",
        ),
        OperationReceipt::ExpectedParentMismatch { .. } => {
            (StatusCode::CONFLICT, "expected_parent_mismatch")
        }
        OperationReceipt::CycleDetected { .. } => (StatusCode::CONFLICT, "cycle_detected"),
        OperationReceipt::RootImmutable { .. } => (StatusCode::CONFLICT, "root_immutable"),
        OperationReceipt::DestinationExists { .. } => (StatusCode::CONFLICT, "destination_exists"),
        OperationReceipt::NodeIdConflict { .. } => (StatusCode::CONFLICT, "node_id_conflict"),
        OperationReceipt::ParentNotFolder { .. } => (StatusCode::CONFLICT, "parent_not_folder"),
        OperationReceipt::NotFound { .. } => (StatusCode::NOT_FOUND, "not_found"),
        OperationReceipt::NotApplied { .. } => (StatusCode::CONFLICT, "operation_not_applied"),
        OperationReceipt::RecoveryConflict { .. } => (StatusCode::CONFLICT, "recovery_conflict"),
        OperationReceipt::Pending { .. } => (StatusCode::SERVICE_UNAVAILABLE, "recovery_pending"),
    };
    let error = error_body(code);
    (
        status,
        Json(OperationError {
            code: error.code,
            message: error.message,
            request_id: error.request_id,
            operation,
        }),
    )
        .into_response()
}
