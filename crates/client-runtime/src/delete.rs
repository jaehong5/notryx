use crate::{
    Binding, Error, OperationLookup, Session,
    write::{api_error, response_body},
};
use notryx_domain::{ChangeSeq, NodeId, OperationId};
use notryx_protocol::{
    AbandonRequest, ApiError, NodeKind, OperationError, OperationReceipt, StrongEtag,
};
use reqwest::{StatusCode, header};
use std::future::Future;

#[derive(Clone)]
pub struct DeleteMutation {
    pub binding: Binding,
    pub operation_id: OperationId,
    pub operation_seq: u64,
    pub node_id: NodeId,
    pub expected_node_etag: String,
}

impl DeleteMutation {
    fn validate(&self, session: &Session) -> Result<(), Error> {
        if self.binding != session.binding {
            return Err(Error::Binding);
        }
        if self.operation_seq == 0
            || self.expected_node_etag.len() > 256
            || StrongEtag::parse(&self.expected_node_etag).is_err()
        {
            return Err(Error::InvalidRequest);
        }
        Ok(())
    }

    fn verify_receipt(&self, receipt: &OperationReceipt) -> Result<(), Error> {
        use OperationReceipt::*;
        let (id, seq, epoch) = match receipt {
            Deleted {
                operation_id,
                operation_seq,
                workspace_epoch,
                node,
                commit_seq,
            } => {
                if node.node_id != self.node_id
                    || !node.deleted
                    || node.parent_id.is_none()
                    || commit_seq.get() == 0
                    || node.last_change_seq != *commit_seq
                    || StrongEtag::from_meta_revision(&node.meta_rev)
                        .map_err(|_| Error::Contract)?
                        .as_str()
                        != node.meta_etag
                    || StrongEtag::parse(&node.node_etag).is_err()
                {
                    return Err(Error::Contract);
                }
                if node.kind == NodeKind::File {
                    let revision = node.content_rev.as_ref().ok_or(Error::Contract)?;
                    let etag =
                        StrongEtag::from_content_revision(revision).map_err(|_| Error::Contract)?;
                    let hash = node
                        .content_hash
                        .as_deref()
                        .and_then(|value| value.strip_prefix("sha256:"))
                        .ok_or(Error::Contract)?;
                    if node.content_etag.as_deref() != Some(etag.as_str())
                        || hash.len() != 64
                        || !hash
                            .bytes()
                            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                        || node
                            .size
                            .as_deref()
                            .and_then(|size| size.parse::<u64>().ok())
                            .is_none()
                    {
                        return Err(Error::Contract);
                    }
                }
                (operation_id, operation_seq, workspace_epoch)
            }
            NodePreconditionFailed {
                operation_id,
                operation_seq,
                workspace_epoch,
                current_node_etag,
            } => {
                if current_node_etag.len() > 256
                    || StrongEtag::parse(current_node_etag).is_err()
                    || current_node_etag == &self.expected_node_etag
                {
                    return Err(Error::Contract);
                }
                (operation_id, operation_seq, workspace_epoch)
            }
            NodeDeleted {
                operation_id,
                operation_seq,
                workspace_epoch,
            }
            | FolderNotEmpty {
                operation_id,
                operation_seq,
                workspace_epoch,
            }
            | RootImmutable {
                operation_id,
                operation_seq,
                workspace_epoch,
            }
            | NotFound {
                operation_id,
                operation_seq,
                workspace_epoch,
            }
            | NotApplied {
                operation_id,
                operation_seq,
                workspace_epoch,
            }
            | Abandoned {
                operation_id,
                operation_seq,
                workspace_epoch,
            }
            | RecoveryConflict {
                operation_id,
                operation_seq,
                workspace_epoch,
            }
            | Pending {
                operation_id,
                operation_seq,
                workspace_epoch,
            } => (operation_id, operation_seq, workspace_epoch),
            _ => return Err(Error::Contract),
        };
        if id != &self.operation_id
            || seq.get() != self.operation_seq
            || epoch != &self.binding.workspace_epoch
        {
            return Err(Error::Binding);
        }
        Ok(())
    }
}

fn matches_error(status: StatusCode, code: &str, receipt: &OperationReceipt) -> bool {
    use OperationReceipt::*;
    match receipt {
        NodePreconditionFailed { .. } => {
            status == StatusCode::PRECONDITION_FAILED && code == "node_revision_mismatch"
        }
        NodeDeleted { .. } => status == StatusCode::GONE && code == "node_deleted",
        FolderNotEmpty { .. } => status == StatusCode::CONFLICT && code == "folder_not_empty",
        RootImmutable { .. } => status == StatusCode::CONFLICT && code == "root_immutable",
        NotFound { .. } => status == StatusCode::NOT_FOUND && code == "not_found",
        NotApplied { .. } => status == StatusCode::CONFLICT && code == "operation_not_applied",
        RecoveryConflict { .. } => status == StatusCode::CONFLICT && code == "recovery_conflict",
        Pending { .. } => status == StatusCode::SERVICE_UNAVAILABLE && code == "recovery_pending",
        _ => false,
    }
}

async fn mutation_response(
    response: reqwest::Response,
    request: &DeleteMutation,
    abandon: bool,
) -> Result<OperationReceipt, Error> {
    let (status, bytes) = response_body(response).await?;
    let receipt = if status == StatusCode::OK {
        let receipt =
            serde_json::from_slice::<OperationReceipt>(&bytes).map_err(|_| Error::Contract)?;
        if !matches!(receipt, OperationReceipt::Deleted { .. })
            && !(abandon && matches!(receipt, OperationReceipt::Abandoned { .. }))
        {
            return Err(Error::Contract);
        }
        receipt
    } else if let Ok(error) = serde_json::from_slice::<OperationError>(&bytes) {
        if !matches_error(status, &error.code, &error.operation) {
            return Err(Error::Contract);
        }
        error.operation
    } else {
        let error = serde_json::from_slice::<ApiError>(&bytes).map_err(|_| Error::Contract)?;
        return Err(api_error(status, &error));
    };
    request.verify_receipt(&receipt)?;
    Ok(receipt)
}

impl Session {
    pub async fn delete_operation(
        &self,
        request: &DeleteMutation,
    ) -> Result<OperationLookup, Error> {
        request.validate(self)?;
        let path = format!("/v1/operations/{}", request.operation_id);
        let (status, body) = response_body(self.get(&path).await?).await?;
        if status == StatusCode::OK {
            let receipt =
                serde_json::from_slice::<OperationReceipt>(&body).map_err(|_| Error::Contract)?;
            request.verify_receipt(&receipt)?;
            return Ok(OperationLookup::Receipt(Box::new(receipt)));
        }
        let error = serde_json::from_slice::<ApiError>(&body).map_err(|_| Error::Contract)?;
        match api_error(status, &error) {
            Error::OperationNotFound => Ok(OperationLookup::Missing),
            Error::OperationRetired => Ok(OperationLookup::Retired),
            error => Err(error),
        }
    }

    pub async fn delete(&self, request: &DeleteMutation) -> Result<OperationReceipt, Error> {
        self.prepare_delete(request)?.await
    }

    pub fn prepare_delete<'a>(
        &'a self,
        request: &'a DeleteMutation,
    ) -> Result<impl Future<Output = Result<OperationReceipt, Error>> + 'a, Error> {
        request.validate(self)?;
        let path = format!("/v1/nodes/{}", request.node_id);
        let prepared = self
            .client
            .delete(self.origin.join(&path).map_err(|_| Error::InvalidRequest)?)
            .header(header::AUTHORIZATION, self.authorization.clone())
            .header(header::IF_MATCH, &request.expected_node_etag)
            .header(
                "x-workspace-epoch",
                request.binding.workspace_epoch.to_string(),
            )
            .header(
                "x-device-stream",
                request.binding.device_stream_id.to_string(),
            )
            .header("x-operation-seq", request.operation_seq.to_string())
            .header("idempotency-key", request.operation_id.to_string())
            .build()
            .map_err(|_| Error::InvalidRequest)?;
        Ok(async move {
            let response = self
                .client
                .execute(prepared)
                .await
                .map_err(|_| Error::Network)?;
            mutation_response(response, request, false).await
        })
    }

    pub async fn abandon_delete(
        &self,
        request: &DeleteMutation,
    ) -> Result<OperationReceipt, Error> {
        self.prepare_abandon_delete(request)?.await
    }

    pub fn prepare_abandon_delete<'a>(
        &'a self,
        request: &'a DeleteMutation,
    ) -> Result<impl Future<Output = Result<OperationReceipt, Error>> + 'a, Error> {
        request.validate(self)?;
        let body = AbandonRequest {
            workspace_epoch: request.binding.workspace_epoch,
            device_stream_id: request.binding.device_stream_id.to_string(),
            operation_id: request.operation_id,
            operation_seq: ChangeSeq::new(request.operation_seq),
        };
        let prepared = self
            .client
            .post(
                self.origin
                    .join("/v1/operation-stream/abandon")
                    .map_err(|_| Error::InvalidOrigin)?,
            )
            .header(header::AUTHORIZATION, self.authorization.clone())
            .json(&body)
            .build()
            .map_err(|_| Error::InvalidRequest)?;
        Ok(async move {
            let response = self
                .client
                .execute(prepared)
                .await
                .map_err(|_| Error::Network)?;
            mutation_response(response, request, true).await
        })
    }
}
