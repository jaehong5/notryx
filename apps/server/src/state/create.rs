use super::{State, db_error, encode, node::node_post_state, stream};
use crate::{CreateNode, Error, NodeSnapshot, Outcome, Receipt};
use notryx_domain::OperationId;
use notryx_protocol::NodeKind;
use serde::{Deserialize, Serialize};
use sqlx::{Connection, SqliteConnection};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreateIntent {
    pub operation_id: OperationId,
    pub parent_before: NodeSnapshot,
    pub created: NodeSnapshot,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination_parent: Option<notryx_server_storage::FolderIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upload_id: Option<uuid::Uuid>,
}

impl State {
    pub async fn admit_create(
        &mut self,
        request: &CreateNode,
        fingerprint: &str,
        outcome: &Outcome,
        intent: Option<&CreateIntent>,
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
            if let Some(id) = intent.upload_id {
                let changed=sqlx::query("UPDATE uploads SET pending_operation_id=? WHERE upload_id=? AND status='ready' AND pending_operation_id IS NULL")
                    .bind(intent.operation_id.to_string()).bind(id.to_string()).execute(&mut *tx).await.map_err(db_error)?.rows_affected();
                if changed != 1 {
                    return Err(Error::UploadUnavailable);
                }
            }
        }
        tx.commit().await.map_err(db_error)
    }

    pub async fn finish_create(
        &mut self,
        intent: &CreateIntent,
        applied: bool,
    ) -> Result<Receipt, Error> {
        let mut tx = self.connection.begin().await.map_err(db_error)?;
        let outcome = if applied {
            let node = &intent.created;
            let seq = node.last_change_seq.get().to_string();
            let mut parent = intent.parent_before.clone();
            parent.children_generation = parent
                .children_generation
                .checked_add(1)
                .ok_or(Error::InvalidFormat)?;
            parent.last_change_seq = node.last_change_seq;
            let changed = sqlx::query("UPDATE nodes SET children_generation=?,last_change_seq=? WHERE node_id=? AND kind='folder' AND meta_rev=? AND children_generation=? AND last_change_seq=?")
                .bind(parent.children_generation.to_string()).bind(&seq).bind(parent.node_id.to_string())
                .bind(parent.meta_rev.as_str()).bind(intent.parent_before.children_generation.to_string()).bind(intent.parent_before.last_change_seq.get().to_string())
                .execute(&mut *tx).await.map_err(db_error)?.rows_affected();
            if changed != 1 {
                return Err(Error::InvalidFormat);
            }
            insert_node(&mut tx, node).await?;
            sqlx::query("INSERT INTO change_sets(seq,operation_id,committed_at) VALUES(?,?,NULL)")
                .bind(&seq)
                .bind(intent.operation_id.to_string())
                .execute(&mut *tx)
                .await
                .map_err(db_error)?;
            for changed_node in [node, &parent] {
                sqlx::query("INSERT INTO node_changes VALUES(?,?,?)")
                    .bind(&seq)
                    .bind(changed_node.node_id.to_string())
                    .bind(encode(&node_post_state(changed_node))?)
                    .execute(&mut *tx)
                    .await
                    .map_err(db_error)?;
            }
            let changed =
                sqlx::query("UPDATE workspaces SET head_seq=? WHERE singleton=1 AND head_seq=?")
                    .bind(&seq)
                    .bind(
                        node.last_change_seq
                            .get()
                            .checked_sub(1)
                            .ok_or(Error::InvalidFormat)?
                            .to_string(),
                    )
                    .execute(&mut *tx)
                    .await
                    .map_err(db_error)?
                    .rows_affected();
            if changed != 1 {
                return Err(Error::InvalidFormat);
            }
            if let Some(id) = intent.upload_id {
                let changed=sqlx::query("UPDATE uploads SET status='consumed',pending_operation_id=NULL WHERE upload_id=? AND status='ready' AND pending_operation_id=?")
                    .bind(id.to_string()).bind(intent.operation_id.to_string()).execute(&mut *tx).await.map_err(db_error)?.rows_affected();
                if changed != 1 {
                    return Err(Error::InvalidFormat);
                }
            }
            Outcome::Created(node.clone())
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

pub(super) async fn insert_node(
    connection: &mut SqliteConnection,
    node: &NodeSnapshot,
) -> Result<(), Error> {
    sqlx::query("INSERT INTO nodes VALUES(?,?,?,?,?,?,?,?,?,?,?,0)")
        .bind(node.node_id.to_string())
        .bind(node.parent_id.map(|id| id.to_string()))
        .bind(&node.name)
        .bind(notryx_domain::mutation::normalized_name_key(&node.name))
        .bind(match node.kind {
            NodeKind::File => "file",
            NodeKind::Folder => "folder",
        })
        .bind(node.content_rev.as_ref().map(|revision| revision.as_str()))
        .bind(node.meta_rev.as_str())
        .bind(&node.content_hash)
        .bind(node.size.map(|size| size.to_string()))
        .bind(node.children_generation.to_string())
        .bind(node.last_change_seq.get().to_string())
        .execute(connection)
        .await
        .map_err(db_error)?;
    Ok(())
}

impl State {
    pub async fn finish_external_create(
        &mut self,
        parent_before: &NodeSnapshot,
        node: &NodeSnapshot,
    ) -> Result<(), Error> {
        let mut parent = parent_before.clone();
        parent.children_generation = parent
            .children_generation
            .checked_add(1)
            .ok_or(Error::InvalidFormat)?;
        parent.last_change_seq = node.last_change_seq;
        let seq = node.last_change_seq.get();
        let mut tx = self.connection.begin().await.map_err(db_error)?;
        let changed = sqlx::query("UPDATE nodes SET children_generation=?,last_change_seq=? WHERE node_id=? AND kind='folder' AND deleted=0 AND meta_rev=? AND children_generation=? AND last_change_seq=?")
            .bind(parent.children_generation.to_string()).bind(seq.to_string()).bind(parent.node_id.to_string())
            .bind(parent_before.meta_rev.as_str()).bind(parent_before.children_generation.to_string()).bind(parent_before.last_change_seq.get().to_string())
            .execute(&mut *tx).await.map_err(db_error)?.rows_affected();
        if changed != 1 {
            return Err(Error::ExternalChange);
        }
        insert_node(&mut tx, node).await?;
        sqlx::query("INSERT INTO change_sets(seq,operation_id,committed_at) VALUES(?,NULL,NULL)")
            .bind(seq.to_string())
            .execute(&mut *tx)
            .await
            .map_err(db_error)?;
        for entry in [node, &parent] {
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
            return Err(Error::ExternalChange);
        }
        tx.commit().await.map_err(db_error)
    }
}
