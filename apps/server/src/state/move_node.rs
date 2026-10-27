use super::{State, db_error, encode, node::node_post_state, stream};
use crate::{Error, MoveNode, NodeSnapshot, Outcome, Receipt};
use notryx_domain::OperationId;
use notryx_server_storage::EntryStamp;
use serde::{Deserialize, Serialize};
use sqlx::{Connection, SqliteConnection};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MoveIntent {
    pub operation_id: OperationId,
    pub before: NodeSnapshot,
    pub proposed: NodeSnapshot,
    pub parents_before: Vec<NodeSnapshot>,
    pub old_path: String,
    pub new_path: String,
    pub source_stamp: EntryStamp,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub observed_hashes: Vec<String>,
}

impl State {
    pub async fn admit_move(
        &mut self,
        request: &MoveNode,
        fingerprint: &str,
        outcome: &Outcome,
        intent: Option<&MoveIntent>,
    ) -> Result<(), Error> {
        let mut tx = self.connection.begin().await.map_err(db_error)?;
        stream::insert_receipt(
            &mut tx,
            request.stream_id,
            request.operation_seq,
            request.operation_id,
            fingerprint,
            outcome,
        )
        .await?;
        if let Some(intent) = intent {
            sqlx::query("INSERT INTO write_intents(operation_id,intent) VALUES(?,?)")
                .bind(request.operation_id.to_string())
                .bind(encode(intent)?)
                .execute(&mut *tx)
                .await
                .map_err(db_error)?;
        }
        tx.commit().await.map_err(db_error)
    }

    pub async fn quarantine_move(
        &mut self,
        intent: &MoveIntent,
        hashes: Vec<String>,
    ) -> Result<(), Error> {
        let mut observed = intent.clone();
        observed.observed_hashes = hashes;
        self.quarantine_intent(
            intent.operation_id,
            None,
            Some(&super::PendingIntent::Move(observed)),
        )
        .await
    }

    pub async fn finish_move(
        &mut self,
        intent: &MoveIntent,
        applied: bool,
    ) -> Result<Receipt, Error> {
        let mut tx = self.connection.begin().await.map_err(db_error)?;
        let outcome = if applied {
            let node = &intent.proposed;
            let seq = node.last_change_seq.get().to_string();
            let changed = sqlx::query("UPDATE nodes SET parent_id=?,name=?,normalized_name_key=?,meta_rev=?,last_change_seq=? WHERE node_id=? AND parent_id=? AND name=? AND meta_rev=? AND last_change_seq=?")
                .bind(node.parent_id.map(|id|id.to_string())).bind(&node.name)
                .bind(notryx_domain::mutation::normalized_name_key(&node.name))
                .bind(node.meta_rev.as_str()).bind(&seq).bind(node.node_id.to_string())
                .bind(intent.before.parent_id.map(|id|id.to_string())).bind(&intent.before.name)
                .bind(intent.before.meta_rev.as_str()).bind(intent.before.last_change_seq.get().to_string())
                .execute(&mut *tx).await.map_err(db_error)?.rows_affected();
            if changed != 1 {
                return Err(Error::InvalidFormat);
            }
            sqlx::query("INSERT INTO change_sets VALUES(?,?)")
                .bind(&seq)
                .bind(intent.operation_id.to_string())
                .execute(&mut *tx)
                .await
                .map_err(db_error)?;
            record_post(&mut tx, node).await?;
            for before in &intent.parents_before {
                let mut parent = before.clone();
                parent.children_generation = parent
                    .children_generation
                    .checked_add(1)
                    .ok_or(Error::InvalidFormat)?;
                parent.last_change_seq = node.last_change_seq;
                let changed = sqlx::query("UPDATE nodes SET children_generation=?,last_change_seq=? WHERE node_id=? AND kind='folder' AND meta_rev=? AND children_generation=? AND last_change_seq=?")
                    .bind(parent.children_generation.to_string()).bind(&seq).bind(parent.node_id.to_string())
                    .bind(before.meta_rev.as_str()).bind(before.children_generation.to_string()).bind(before.last_change_seq.get().to_string())
                    .execute(&mut *tx).await.map_err(db_error)?.rows_affected();
                if changed != 1 {
                    return Err(Error::InvalidFormat);
                }
                record_post(&mut tx, &parent).await?;
            }
            let previous = node
                .last_change_seq
                .get()
                .checked_sub(1)
                .ok_or(Error::InvalidFormat)?;
            let changed =
                sqlx::query("UPDATE workspaces SET head_seq=? WHERE singleton=1 AND head_seq=?")
                    .bind(&seq)
                    .bind(previous.to_string())
                    .execute(&mut *tx)
                    .await
                    .map_err(db_error)?
                    .rows_affected();
            if changed != 1 {
                return Err(Error::InvalidFormat);
            }
            Outcome::Moved(node.clone())
        } else {
            Outcome::NotApplied
        };
        stream::finish_pending(&mut tx, intent.operation_id, &outcome).await?;
        tx.commit().await.map_err(db_error)?;
        Ok(Receipt {
            operation_id: intent.operation_id,
            outcome,
        })
    }
}

async fn record_post(connection: &mut SqliteConnection, node: &NodeSnapshot) -> Result<(), Error> {
    sqlx::query("INSERT INTO node_changes VALUES(?,?,?)")
        .bind(node.last_change_seq.get().to_string())
        .bind(node.node_id.to_string())
        .bind(encode(&node_post_state(node))?)
        .execute(connection)
        .await
        .map_err(db_error)?;
    Ok(())
}
