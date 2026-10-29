use crate::{
    AbandonOperation, AckOperations, Error, OperationRecord, OperationStreamState, Outcome,
    Workspace,
};

impl Workspace {
    pub(crate) async fn ack_operations(
        &mut self,
        request: AckOperations,
    ) -> Result<OperationStreamState, Error> {
        if request.epoch != self.epoch() {
            return Err(Error::WorkspaceMismatch);
        }
        if self.journal_integrity_failed {
            return Err(Error::RecoveryRequired);
        }
        self.state
            .ack_operations(
                request.stream_id,
                request.ack_through,
                #[cfg(test)]
                &mut self.hook,
            )
            .await
    }

    pub(crate) async fn abandon_operation(
        &mut self,
        request: AbandonOperation,
    ) -> Result<OperationRecord, Error> {
        if request.epoch != self.epoch() {
            return Err(Error::WorkspaceMismatch);
        }
        if request.operation_seq == 0 {
            return Err(Error::InvalidRequest);
        }
        if let Some(previous) = self
            .state
            .previous_identity(
                request.stream_id,
                request.operation_seq,
                request.operation_id,
                None,
            )
            .await?
        {
            if previous.outcome == Outcome::Pending {
                return Err(Error::OperationPending);
            }
        } else {
            if !self.ready() {
                return Err(Error::RecoveryRequired);
            }
            self.verify_mutation_root()?;
            self.state
                .validate_admission(request.stream_id, request.operation_seq)
                .await?;
            let fingerprint = crate::hash(
                &serde_json::to_vec(&(
                    "abandon_operation/v1",
                    request.epoch,
                    request.stream_id,
                    request.operation_seq,
                    request.operation_id,
                ))
                .map_err(crate::state::serialization_error)?,
            );
            // An uncertain admission must keep later writes blocked until restart validates the DB.
            self.ready = false;
            self.state.abandon_operation(&request, &fingerprint).await?;
            self.ready = true;
        }
        self.state
            .operation(request.stream_id, request.operation_id)
            .await
    }
}
