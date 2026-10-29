use crate::{
    Binding, Error, MAX_EDITABLE_DOCUMENT_BYTES, MAX_JSON_BYTES, Session, bounded_body,
    check_response, content_type, json,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use notryx_domain::{ChangeSeq, ContentRevision, NodeId, OperationId};
use notryx_protocol::{
    AbandonRequest, AckRequest, AckResponse, ApiError, OperationError, OperationReceipt, StrongEtag,
};
use reqwest::{Response, StatusCode, header};
use sha2::{Digest, Sha256};
use std::future::Future;

#[derive(Clone)]
pub struct ContentMutation {
    pub binding: Binding,
    pub operation_id: OperationId,
    pub operation_seq: u64,
    pub node_id: NodeId,
    pub expected_revision: ContentRevision,
    pub bytes: Vec<u8>,
}

#[derive(Clone)]
pub struct AbandonMutation {
    pub binding: Binding,
    pub operation_id: OperationId,
    pub operation_seq: u64,
    pub node_id: NodeId,
    pub bytes: Vec<u8>,
}

struct ReceiptProof<'a> {
    binding: &'a Binding,
    operation_id: OperationId,
    operation_seq: u64,
    node_id: NodeId,
    bytes: &'a [u8],
}

#[derive(Debug, PartialEq, Eq)]
pub enum OperationLookup {
    Receipt(Box<OperationReceipt>),
    Missing,
    Retired,
}

impl ContentMutation {
    fn validate(&self, session: &Session) -> Result<(), Error> {
        if self.binding != session.binding {
            return Err(Error::Binding);
        }
        if self.operation_seq == 0 {
            return Err(Error::InvalidRequest);
        }
        if self.bytes.len() > MAX_EDITABLE_DOCUMENT_BYTES {
            return Err(Error::TooLarge);
        }
        StrongEtag::from_content_revision(&self.expected_revision)
            .map_err(|_| Error::InvalidRequest)?;
        Ok(())
    }

    fn verify_receipt(&self, receipt: &OperationReceipt) -> Result<(), Error> {
        if let OperationReceipt::PreconditionFailed {
            current_content_rev,
            ..
        } = receipt
            && current_content_rev == &self.expected_revision
        {
            return Err(Error::Contract);
        }
        ReceiptProof {
            binding: &self.binding,
            operation_id: self.operation_id,
            operation_seq: self.operation_seq,
            node_id: self.node_id,
            bytes: &self.bytes,
        }
        .verify_receipt(receipt)
    }
}

impl ReceiptProof<'_> {
    fn content_hash(&self) -> String {
        let hex = Sha256::digest(self.bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        format!("sha256:{hex}")
    }

    fn verify_receipt(&self, receipt: &OperationReceipt) -> Result<(), Error> {
        use OperationReceipt::*;
        let (operation_id, operation_seq, workspace_epoch) = match receipt {
            Committed {
                operation_id,
                operation_seq,
                workspace_epoch,
                node_id,
                content_rev,
                content_etag,
                meta_rev,
                meta_etag,
                node_etag,
                content_hash,
                ..
            } => {
                if node_id != &self.node_id
                    || content_hash != &self.content_hash()
                    || StrongEtag::from_content_revision(content_rev)
                        .map_err(|_| Error::Contract)?
                        .as_str()
                        != content_etag
                    || StrongEtag::from_meta_revision(meta_rev)
                        .map_err(|_| Error::Contract)?
                        .as_str()
                        != meta_etag
                    || StrongEtag::parse(node_etag).is_err()
                {
                    return Err(Error::Contract);
                }
                (operation_id, operation_seq, workspace_epoch)
            }
            PreconditionFailed {
                operation_id,
                operation_seq,
                workspace_epoch,
                current_content_rev,
                current_content_etag,
            } => {
                if StrongEtag::from_content_revision(current_content_rev)
                    .map_err(|_| Error::Contract)?
                    .as_str()
                    != current_content_etag
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
            | NotFound {
                operation_id,
                operation_seq,
                workspace_epoch,
            }
            | RecoveryConflict {
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
            | Pending {
                operation_id,
                operation_seq,
                workspace_epoch,
            } => (operation_id, operation_seq, workspace_epoch),
            _ => return Err(Error::Contract),
        };
        if operation_id != &self.operation_id
            || operation_seq.get() != self.operation_seq
            || workspace_epoch != &self.binding.workspace_epoch
        {
            return Err(Error::Binding);
        }
        Ok(())
    }
}

fn receipt_status_matches(status: StatusCode, code: &str, receipt: &OperationReceipt) -> bool {
    use OperationReceipt::*;
    match receipt {
        PreconditionFailed { .. } => {
            status == StatusCode::PRECONDITION_FAILED && code == "content_revision_mismatch"
        }
        NodeDeleted { .. } => status == StatusCode::GONE && code == "node_deleted",
        NotFound { .. } => status == StatusCode::NOT_FOUND && code == "not_found",
        RecoveryConflict { .. } => status == StatusCode::CONFLICT && code == "recovery_conflict",
        NotApplied { .. } => status == StatusCode::CONFLICT && code == "operation_not_applied",
        Pending { .. } => status == StatusCode::SERVICE_UNAVAILABLE && code == "recovery_pending",
        _ => false,
    }
}

pub(crate) fn api_error(status: StatusCode, error: &ApiError) -> Error {
    match (status.as_u16(), error.code.as_str()) {
        (404, "not_found") => Error::OperationNotFound,
        (410, "operation_retired") => Error::OperationRetired,
        (409, "idempotency_mismatch") => Error::IdempotencyMismatch,
        (409, "operation_gap") => Error::OperationGap,
        (409, "epoch_mismatch") => Error::EpochMismatch,
        (409, "unknown_stream") => Error::Binding,
        (409, "operation_pending") => Error::Unavailable,
        (429 | 503 | 507, _) => Error::Unavailable,
        _ => Error::Response,
    }
}

pub(crate) async fn response_body(response: Response) -> Result<(StatusCode, Vec<u8>), Error> {
    let status = response.status();
    let media_type = content_type(&response)?;
    let gateway_response = status.is_redirection()
        || matches!(status.as_u16(), 401 | 403)
        || media_type.is_some_and(|value| {
            value.eq_ignore_ascii_case("text/html")
                || value.eq_ignore_ascii_case("application/xhtml+xml")
        });
    if gateway_response {
        return match check_response(response).await {
            Err(error) => Err(error),
            Ok(_) => Err(Error::Response),
        };
    }
    if !media_type.is_some_and(|value| value.eq_ignore_ascii_case("application/json")) {
        return Err(Error::Contract);
    }
    Ok((status, bounded_body(response, MAX_JSON_BYTES).await?))
}

impl Session {
    pub async fn abandon(&self, request: &AbandonMutation) -> Result<OperationReceipt, Error> {
        if request.binding != self.binding {
            return Err(Error::Binding);
        }
        if request.operation_seq == 0 {
            return Err(Error::InvalidRequest);
        }
        if request.bytes.len() > MAX_EDITABLE_DOCUMENT_BYTES {
            return Err(Error::TooLarge);
        }
        let body = AbandonRequest {
            workspace_epoch: request.binding.workspace_epoch,
            device_stream_id: request.binding.device_stream_id.to_string(),
            operation_id: request.operation_id,
            operation_seq: ChangeSeq::new(request.operation_seq),
        };
        let response = self
            .client
            .post(
                self.origin
                    .join("/v1/operation-stream/abandon")
                    .map_err(|_| Error::InvalidOrigin)?,
            )
            .header(header::AUTHORIZATION, self.authorization.clone())
            .json(&body)
            .send()
            .await
            .map_err(|_| Error::Network)?;
        let (status, bytes) = response_body(response).await?;
        let receipt = if status == StatusCode::OK {
            let receipt =
                serde_json::from_slice::<OperationReceipt>(&bytes).map_err(|_| Error::Contract)?;
            if !matches!(
                receipt,
                OperationReceipt::Abandoned { .. } | OperationReceipt::Committed { .. }
            ) {
                return Err(Error::Contract);
            }
            receipt
        } else if let Ok(error) = serde_json::from_slice::<OperationError>(&bytes) {
            if !receipt_status_matches(status, &error.code, &error.operation) {
                return Err(Error::Contract);
            }
            error.operation
        } else {
            let error = serde_json::from_slice::<ApiError>(&bytes).map_err(|_| Error::Contract)?;
            return Err(api_error(status, &error));
        };
        ReceiptProof {
            binding: &request.binding,
            operation_id: request.operation_id,
            operation_seq: request.operation_seq,
            node_id: request.node_id,
            bytes: &request.bytes,
        }
        .verify_receipt(&receipt)?;
        Ok(receipt)
    }

    pub async fn put_content(&self, request: &ContentMutation) -> Result<OperationReceipt, Error> {
        self.prepare_put_content(request)?.await
    }

    pub fn prepare_put_content<'a>(
        &'a self,
        request: &'a ContentMutation,
    ) -> Result<impl Future<Output = Result<OperationReceipt, Error>> + 'a, Error> {
        request.validate(self)?;
        let etag = StrongEtag::from_content_revision(&request.expected_revision)
            .map_err(|_| Error::InvalidRequest)?;
        let digest = format!(
            "sha-256=:{}:",
            STANDARD.encode(Sha256::digest(&request.bytes))
        );
        let path = format!("/v1/nodes/{}/content", request.node_id);
        let prepared = self
            .client
            .put(self.origin.join(&path).map_err(|_| Error::InvalidRequest)?)
            .header(header::AUTHORIZATION, self.authorization.clone())
            .header(header::IF_MATCH, etag.as_str())
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .header("content-digest", digest)
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
            .body(request.bytes.clone())
            .build()
            .map_err(|_| Error::InvalidRequest)?;
        Ok(async move {
            let response = self
                .client
                .execute(prepared)
                .await
                .map_err(|_| Error::Network)?;
            let (status, body) = response_body(response).await?;
            let receipt = if status == StatusCode::OK {
                let receipt = serde_json::from_slice::<OperationReceipt>(&body)
                    .map_err(|_| Error::Contract)?;
                if !matches!(
                    receipt,
                    OperationReceipt::Committed { .. } | OperationReceipt::Abandoned { .. }
                ) {
                    return Err(Error::Contract);
                }
                receipt
            } else if let Ok(error) = serde_json::from_slice::<OperationError>(&body) {
                if !receipt_status_matches(status, &error.code, &error.operation) {
                    return Err(Error::Contract);
                }
                error.operation
            } else {
                let error =
                    serde_json::from_slice::<ApiError>(&body).map_err(|_| Error::Contract)?;
                return Err(api_error(status, &error));
            };
            request.verify_receipt(&receipt)?;
            Ok(receipt)
        })
    }

    pub async fn operation(&self, request: &ContentMutation) -> Result<OperationLookup, Error> {
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

    pub async fn acknowledge(&self, through: u64) -> Result<AckResponse, Error> {
        let request = AckRequest {
            workspace_epoch: self.binding.workspace_epoch,
            device_stream_id: self.binding.device_stream_id.to_string(),
            ack_through: ChangeSeq::new(through),
        };
        let response = self
            .client
            .post(
                self.origin
                    .join("/v1/operation-stream/ack")
                    .map_err(|_| Error::InvalidOrigin)?,
            )
            .header(header::AUTHORIZATION, self.authorization.clone())
            .json(&request)
            .send()
            .await
            .map_err(|_| Error::Network)?;
        let ack: AckResponse = json(response).await?;
        if ack.workspace_epoch != self.binding.workspace_epoch
            || ack.device_stream_id != request.device_stream_id
            || ack.retired_through.get() != through
            || ack.retired_through.get() > ack.last_seq.get()
        {
            return Err(Error::Binding);
        }
        Ok(ack)
    }
}
