use super::{State, db_error, encode, node::node_post_state, stream};
use crate::{Error, NodeSnapshot, Outcome, Receipt};
use notryx_domain::OperationId;
use notryx_server_storage::EntryStamp;
use serde::{Deserialize, Serialize};
use sqlx::Connection;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) enum LifecycleAction {
    Delete { source_stamp: EntryStamp },
    Restore,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LifecycleIntent {
    pub operation_id: OperationId,
    pub before: NodeSnapshot,
    pub proposed: NodeSnapshot,
    pub parent_before: NodeSnapshot,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination_parent: Option<notryx_server_storage::FolderIdentity>,
    pub action: LifecycleAction,
}

impl State {
    pub async fn finish_external_delete(
        &mut self,
        before: &NodeSnapshot,
        proposed: &NodeSnapshot,
        parent_before: &NodeSnapshot,
        watch_generation: &std::sync::atomic::AtomicU64,
        expected_watch_generation: u64,
        #[cfg(test)] pause: Option<(
            tokio::sync::oneshot::Sender<()>,
            tokio::sync::oneshot::Receiver<()>,
        )>,
    ) -> Result<(), Error> {
        let mut parent = parent_before.clone();
        parent.children_generation = parent
            .children_generation
            .checked_add(1)
            .ok_or(Error::InvalidFormat)?;
        parent.last_change_seq = proposed.last_change_seq;
        let seq = proposed.last_change_seq.get();
        let mut tx = self.connection.begin().await.map_err(db_error)?;
        if expected_watch_generation & 1 != 0
            || watch_generation.load(std::sync::atomic::Ordering::Acquire)
                != expected_watch_generation
        {
            tx.rollback().await.map_err(db_error)?;
            return Err(Error::ExternalChange);
        }
        let changed = if before.kind == notryx_protocol::NodeKind::Folder {
            sqlx::query("UPDATE nodes SET meta_rev=?,last_change_seq=?,deleted=1 WHERE node_id=? AND kind='folder' AND deleted=0 AND parent_id=? AND name=? AND meta_rev=? AND content_rev IS NULL AND content_hash IS NULL AND size IS NULL AND children_generation=? AND last_change_seq=? AND NOT EXISTS(SELECT 1 FROM nodes child WHERE child.parent_id=nodes.node_id AND child.deleted=0)")
                .bind(proposed.meta_rev.as_str()).bind(seq.to_string()).bind(before.node_id.to_string())
                .bind(before.parent_id.map(|id| id.to_string())).bind(&before.name).bind(before.meta_rev.as_str())
                .bind(before.children_generation.to_string()).bind(before.last_change_seq.get().to_string())
                .execute(&mut *tx).await.map_err(db_error)?.rows_affected()
        } else {
            sqlx::query("UPDATE nodes SET content_rev=?,meta_rev=?,last_change_seq=?,deleted=1 WHERE node_id=? AND kind='file' AND deleted=0 AND parent_id=? AND name=? AND meta_rev=? AND content_rev=? AND content_hash=? AND size=? AND last_change_seq=?")
                .bind(proposed.content_rev.as_ref().map(|value| value.as_str()))
                .bind(proposed.meta_rev.as_str()).bind(seq.to_string()).bind(before.node_id.to_string())
                .bind(before.parent_id.map(|id| id.to_string())).bind(&before.name).bind(before.meta_rev.as_str())
                .bind(before.content_rev.as_ref().map(|value| value.as_str())).bind(&before.content_hash)
                .bind(before.size.map(|size| size.to_string())).bind(before.last_change_seq.get().to_string())
                .execute(&mut *tx).await.map_err(db_error)?.rows_affected()
        };
        if changed != 1 {
            tx.rollback().await.map_err(db_error)?;
            return Err(Error::ExternalChange);
        }
        let changed = sqlx::query("UPDATE nodes SET children_generation=?,last_change_seq=? WHERE node_id=? AND deleted=0 AND kind='folder' AND meta_rev=? AND children_generation=? AND last_change_seq=?")
            .bind(parent.children_generation.to_string()).bind(seq.to_string()).bind(parent.node_id.to_string())
            .bind(parent_before.meta_rev.as_str()).bind(parent_before.children_generation.to_string()).bind(parent_before.last_change_seq.get().to_string())
            .execute(&mut *tx).await.map_err(db_error)?.rows_affected();
        if changed != 1 {
            tx.rollback().await.map_err(db_error)?;
            return Err(Error::ExternalChange);
        }
        sqlx::query("INSERT INTO change_sets(seq,operation_id,committed_at) VALUES(?,NULL,NULL)")
            .bind(seq.to_string())
            .execute(&mut *tx)
            .await
            .map_err(db_error)?;
        for entry in [proposed, &parent] {
            sqlx::query("INSERT INTO node_changes VALUES(?,?,?)")
                .bind(seq.to_string())
                .bind(entry.node_id.to_string())
                .bind(encode(&node_post_state(entry))?)
                .execute(&mut *tx)
                .await
                .map_err(db_error)?;
        }
        let changed =
            sqlx::query("UPDATE workspaces SET head_seq=? WHERE singleton=1 AND head_seq=?")
                .bind(seq.to_string())
                .bind(seq.checked_sub(1).ok_or(Error::InvalidFormat)?.to_string())
                .execute(&mut *tx)
                .await
                .map_err(db_error)?
                .rows_affected();
        if changed != 1 {
            tx.rollback().await.map_err(db_error)?;
            return Err(Error::ExternalChange);
        }
        #[cfg(test)]
        if let Some((entered, release)) = pause {
            let _ = entered.send(());
            let _ = release.await;
        }
        if watch_generation.load(std::sync::atomic::Ordering::Acquire) != expected_watch_generation
        {
            tx.rollback().await.map_err(db_error)?;
            return Err(Error::ExternalChange);
        }
        tx.commit().await.map_err(db_error)
    }

    pub async fn admit_lifecycle(
        &mut self,
        stream_id: Uuid,
        sequence: u64,
        operation_id: OperationId,
        fingerprint: &str,
        outcome: &Outcome,
        intent: Option<&LifecycleIntent>,
    ) -> Result<(), Error> {
        let mut tx = self.connection.begin().await.map_err(db_error)?;
        stream::insert_receipt(
            &mut tx,
            stream_id,
            sequence,
            operation_id,
            fingerprint,
            outcome,
        )
        .await?;
        if let Some(intent) = intent {
            sqlx::query("INSERT INTO write_intents(operation_id,intent) VALUES(?,?)")
                .bind(operation_id.to_string())
                .bind(encode(intent)?)
                .execute(&mut *tx)
                .await
                .map_err(db_error)?;
        }
        tx.commit().await.map_err(db_error)
    }

    pub async fn finish_lifecycle(
        &mut self,
        intent: &LifecycleIntent,
        applied: bool,
    ) -> Result<Receipt, Error> {
        let mut tx = self.connection.begin().await.map_err(db_error)?;
        let outcome = if applied {
            let node = &intent.proposed;
            let before = &intent.before;
            let seq = node.last_change_seq.get().to_string();
            let changed = sqlx::query("UPDATE nodes SET parent_id=?,name=?,normalized_name_key=?,content_rev=?,meta_rev=?,last_change_seq=?,deleted=? WHERE node_id=? AND deleted=? AND meta_rev=? AND last_change_seq=?")
                .bind(node.parent_id.map(|id|id.to_string())).bind(&node.name)
                .bind(notryx_domain::mutation::normalized_name_key(&node.name))
                .bind(node.content_rev.as_ref().map(|r|r.as_str())).bind(node.meta_rev.as_str()).bind(&seq).bind(node.deleted)
                .bind(node.node_id.to_string()).bind(before.deleted).bind(before.meta_rev.as_str()).bind(before.last_change_seq.get().to_string())
                .execute(&mut *tx).await.map_err(db_error)?.rows_affected();
            if changed != 1 {
                return Err(Error::InvalidFormat);
            }
            let mut parent = intent.parent_before.clone();
            parent.children_generation = parent
                .children_generation
                .checked_add(1)
                .ok_or(Error::InvalidFormat)?;
            parent.last_change_seq = node.last_change_seq;
            let changed = sqlx::query("UPDATE nodes SET children_generation=?,last_change_seq=? WHERE node_id=? AND deleted=0 AND kind='folder' AND meta_rev=? AND children_generation=? AND last_change_seq=?")
                .bind(parent.children_generation.to_string()).bind(&seq).bind(parent.node_id.to_string())
                .bind(parent.meta_rev.as_str()).bind(intent.parent_before.children_generation.to_string()).bind(intent.parent_before.last_change_seq.get().to_string())
                .execute(&mut *tx).await.map_err(db_error)?.rows_affected();
            if changed != 1 {
                return Err(Error::InvalidFormat);
            }
            sqlx::query("INSERT INTO change_sets(seq,operation_id,committed_at) VALUES(?,?,NULL)")
                .bind(&seq)
                .bind(intent.operation_id.to_string())
                .execute(&mut *tx)
                .await
                .map_err(db_error)?;
            for changed in [node, &parent] {
                sqlx::query("INSERT INTO node_changes VALUES(?,?,?)")
                    .bind(&seq)
                    .bind(changed.node_id.to_string())
                    .bind(encode(&node_post_state(changed))?)
                    .execute(&mut *tx)
                    .await
                    .map_err(db_error)?;
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
            match intent.action {
                LifecycleAction::Delete { .. } => Outcome::Deleted(node.clone()),
                LifecycleAction::Restore => Outcome::Restored(node.clone()),
            }
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

    pub async fn has_live_children(
        &mut self,
        node_id: notryx_domain::NodeId,
    ) -> Result<bool, Error> {
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM nodes WHERE parent_id=? AND deleted=0)")
            .bind(node_id.to_string())
            .fetch_one(&mut self.connection)
            .await
            .map_err(db_error)
    }

    pub async fn quarantine_hash(
        &mut self,
        operation_id: OperationId,
    ) -> Result<Option<String>, Error> {
        sqlx::query_scalar("SELECT observed_hash FROM write_intents WHERE operation_id=?")
            .bind(operation_id.to_string())
            .fetch_one(&mut self.connection)
            .await
            .map_err(db_error)
    }
}
