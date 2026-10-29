use crate::{
    Binding, Error, Session,
    write::{api_error, response_body},
};
use notryx_domain::{ChangeSeq, OperationId};
use notryx_protocol::{
    AbandonRequest, ApiError, CreateNodeRequest, NodeKind, OperationError, OperationReceipt,
    StrongEtag,
};
use reqwest::{StatusCode, header};
use std::future::Future;

#[derive(Clone)]
pub struct CreateMutation {
    pub binding: Binding,
    pub operation_id: OperationId,
    pub operation_seq: u64,
    pub request: CreateNodeRequest,
}

impl CreateMutation {
    fn validate(&self, session: &Session) -> Result<(), Error> {
        if self.binding != session.binding {
            return Err(Error::Binding);
        }
        let json = serde_json::to_vec(&self.request).map_err(|_| Error::InvalidRequest)?;
        if self.operation_seq == 0
            || self.request.upload_id.is_some()
            || self.request.node_id == self.request.parent_id
            || serde_json::from_slice::<CreateNodeRequest>(&json)
                .ok()
                .as_ref()
                != Some(&self.request)
        {
            return Err(Error::InvalidRequest);
        }
        Ok(())
    }

    fn verify_receipt(&self, receipt: &OperationReceipt) -> Result<(), Error> {
        use OperationReceipt::*;
        let (id, sequence, epoch) = match receipt {
            Created {
                operation_id,
                operation_seq,
                workspace_epoch,
                node,
                commit_seq,
            } => {
                if node.node_id != self.request.node_id
                    || node.parent_id != Some(self.request.parent_id)
                    || node.kind != self.request.kind
                    || node.name != self.request.name
                    || node.deleted
                    || node.children_generation.get() != 0
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
                match self.request.kind {
                    NodeKind::Folder => {
                        if node.content_rev.is_some()
                            || node.content_etag.is_some()
                            || node.content_hash.is_some()
                            || node.size.is_some()
                        {
                            return Err(Error::Contract);
                        }
                    }
                    NodeKind::File => {
                        let revision = node.content_rev.as_ref().ok_or(Error::Contract)?;
                        if node.content_etag.as_deref()
                            != Some(
                                StrongEtag::from_content_revision(revision)
                                    .map_err(|_| Error::Contract)?
                                    .as_str(),
                            )
                            || node.content_hash.as_deref()
                                != Some(
                                    "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
                                )
                            || node
                                .size
                                .as_deref()
                                .and_then(|size| size.parse::<u64>().ok())
                                != Some(0)
                        {
                            return Err(Error::Contract);
                        }
                    }
                }
                (operation_id, operation_seq, workspace_epoch)
            }
            NodeDeleted {
                operation_id,
                operation_seq,
                workspace_epoch,
            }
            | DestinationExists {
                operation_id,
                operation_seq,
                workspace_epoch,
            }
            | NodeIdConflict {
                operation_id,
                operation_seq,
                workspace_epoch,
            }
            | ParentNotFolder {
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
            || sequence.get() != self.operation_seq
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
        NodeDeleted { .. } => status == StatusCode::GONE && code == "node_deleted",
        DestinationExists { .. } => status == StatusCode::CONFLICT && code == "destination_exists",
        NodeIdConflict { .. } => status == StatusCode::CONFLICT && code == "node_id_conflict",
        ParentNotFolder { .. } => status == StatusCode::CONFLICT && code == "parent_not_folder",
        NotFound { .. } => status == StatusCode::NOT_FOUND && code == "not_found",
        NotApplied { .. } => status == StatusCode::CONFLICT && code == "operation_not_applied",
        RecoveryConflict { .. } => status == StatusCode::CONFLICT && code == "recovery_conflict",
        Pending { .. } => status == StatusCode::SERVICE_UNAVAILABLE && code == "recovery_pending",
        _ => false,
    }
}

fn decode_operation(
    status: StatusCode,
    bytes: &[u8],
    allow_abandoned: bool,
) -> Result<OperationReceipt, Error> {
    if status == StatusCode::OK {
        let receipt =
            serde_json::from_slice::<OperationReceipt>(bytes).map_err(|_| Error::Contract)?;
        if !matches!(receipt, OperationReceipt::Created { .. })
            && !(allow_abandoned && matches!(receipt, OperationReceipt::Abandoned { .. }))
        {
            return Err(Error::Contract);
        }
        Ok(receipt)
    } else if let Ok(error) = serde_json::from_slice::<OperationError>(bytes) {
        if !matches_error(status, &error.code, &error.operation) {
            return Err(Error::Contract);
        }
        Ok(error.operation)
    } else {
        let error = serde_json::from_slice::<ApiError>(bytes).map_err(|_| Error::Contract)?;
        Err(api_error(status, &error))
    }
}

impl Session {
    pub async fn create(&self, request: &CreateMutation) -> Result<OperationReceipt, Error> {
        self.prepare_create(request)?.await
    }

    pub fn prepare_create<'a>(
        &'a self,
        request: &'a CreateMutation,
    ) -> Result<impl Future<Output = Result<OperationReceipt, Error>> + 'a, Error> {
        request.validate(self)?;
        let prepared = self
            .client
            .post(
                self.origin
                    .join("/v1/nodes")
                    .map_err(|_| Error::InvalidOrigin)?,
            )
            .header(header::AUTHORIZATION, self.authorization.clone())
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
            .json(&request.request)
            .build()
            .map_err(|_| Error::InvalidRequest)?;
        Ok(async move {
            let response = self
                .client
                .execute(prepared)
                .await
                .map_err(|_| Error::Network)?;
            let (status, bytes) = response_body(response).await?;
            let receipt = decode_operation(status, &bytes, false)?;
            request.verify_receipt(&receipt)?;
            Ok(receipt)
        })
    }

    pub async fn create_operation(
        &self,
        request: &CreateMutation,
    ) -> Result<crate::OperationLookup, Error> {
        request.validate(self)?;
        let path = format!("/v1/operations/{}", request.operation_id);
        let (status, body) = response_body(self.get(&path).await?).await?;
        if status == StatusCode::OK {
            let receipt =
                serde_json::from_slice::<OperationReceipt>(&body).map_err(|_| Error::Contract)?;
            request.verify_receipt(&receipt)?;
            return Ok(crate::OperationLookup::Receipt(Box::new(receipt)));
        }
        let error = serde_json::from_slice::<ApiError>(&body).map_err(|_| Error::Contract)?;
        match api_error(status, &error) {
            Error::OperationNotFound => Ok(crate::OperationLookup::Missing),
            Error::OperationRetired => Ok(crate::OperationLookup::Retired),
            error => Err(error),
        }
    }

    pub fn prepare_abandon_create<'a>(
        &'a self,
        request: &'a CreateMutation,
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
            let (status, bytes) = response_body(response).await?;
            let receipt = decode_operation(status, &bytes, true)?;
            request.verify_receipt(&receipt)?;
            Ok(receipt)
        })
    }
}

#[cfg(test)]
mod file_receipt_tests {
    use super::*;
    use serde_json::json;
    use uuid::Uuid;

    #[test]
    fn file_receipt_accepts_valid_noncanonical_decimal_zero() {
        let binding = Binding {
            origin: "https://example.invalid".into(),
            workspace_id: Uuid::new_v4().to_string().parse().unwrap(),
            workspace_epoch: Uuid::new_v4().to_string().parse().unwrap(),
            device_id: Uuid::new_v4(),
            device_stream_id: Uuid::new_v4(),
        };
        let request = CreateMutation {
            binding: binding.clone(), operation_id: Uuid::now_v7().to_string().parse().unwrap(), operation_seq: 1,
            request: serde_json::from_value(json!({"node_id":"019a1000-0000-7000-8000-000000000002","parent_id":"019a1000-0000-7000-8000-000000000001","name":"new.md","kind":"file"})).unwrap(),
        };
        let receipt: OperationReceipt = serde_json::from_value(json!({"status":"created","operation_id":request.operation_id.to_string(),"operation_seq":"1","workspace_epoch":binding.workspace_epoch.to_string(),
            "node":{"node_id":request.request.node_id.to_string(),"parent_id":request.request.parent_id.to_string(),"name":"new.md","kind":"file","deleted":false,
              "content_rev":"content-9","content_etag":"\"content-9\"","meta_rev":"meta-9","meta_etag":"\"meta-9\"","node_etag":"\"node-9\"",
              "content_hash":"sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855","size":"00","children_generation":"0","last_change_seq":"9"},"commit_seq":"9"})).unwrap();
        assert!(request.verify_receipt(&receipt).is_ok());
    }
}
