use crate::{Error, Workspace, retention, state};
use notryx_domain::{ChangeSeq, WorkspaceEpoch};
use sqlx::{Connection, Row};

pub(crate) struct JournalStatus {
    pub(crate) workspace_epoch: WorkspaceEpoch,
    pub(crate) head_seq: ChangeSeq,
    pub(crate) journal_floor_seq: ChangeSeq,
    pub(crate) logical_bytes: u64,
    pub(crate) budget_bytes: u64,
    pub(crate) budget_exceeded: bool,
}

fn sequence(text: &str) -> Result<ChangeSeq, Error> {
    let value = text.parse::<u64>().map_err(|_| Error::InvalidFormat)?;
    if value.to_string() != text {
        return Err(Error::InvalidFormat);
    }
    Ok(ChangeSeq::new(value))
}

impl Workspace {
    pub(crate) async fn journal_status(
        &mut self,
        budget_bytes: u64,
    ) -> Result<JournalStatus, Error> {
        let result = self.journal_status_inner(budget_bytes).await;
        if matches!(&result, Err(Error::InvalidFormat)) {
            self.journal_integrity_failed = true;
        }
        result
    }

    async fn journal_status_inner(&mut self, budget_bytes: u64) -> Result<JournalStatus, Error> {
        let mut tx = self
            .state
            .connection
            .begin()
            .await
            .map_err(state::db_error)?;
        let row = sqlx::query(
            "SELECT epoch,head_seq,journal_floor_seq FROM workspaces WHERE singleton=1",
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(state::db_error)?;
        let epoch: String = row.try_get("epoch").map_err(state::db_error)?;
        if epoch != self.state.epoch.to_string() {
            return Err(Error::InvalidFormat);
        }
        let head = sequence(row.try_get("head_seq").map_err(state::db_error)?)?;
        let floor = sequence(row.try_get("journal_floor_seq").map_err(state::db_error)?)?;
        if floor > head {
            return Err(Error::InvalidFormat);
        }
        state::validate_retention(&mut tx, self.state.root_id).await?;
        let logical_bytes = retention::logical_bytes(&mut tx).await?;
        tx.commit().await.map_err(state::db_error)?;
        Ok(JournalStatus {
            workspace_epoch: self.state.epoch,
            head_seq: head,
            journal_floor_seq: floor,
            logical_bytes,
            budget_bytes,
            budget_exceeded: logical_bytes > budget_bytes,
        })
    }
}
