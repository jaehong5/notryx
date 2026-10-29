use crate::{Error, Workspace, state};
use futures_util::TryStreamExt;
use notryx_domain::{ChangeSeq, NodeId};
use sha2::{Digest, Sha256};
use sqlx::{Connection, Row, SqliteConnection};

#[cfg(test)]
mod report_tests;

fn canonical_u64(value: &str) -> Result<u64, Error> {
    let number = value.parse::<u64>().map_err(|_| Error::InvalidFormat)?;
    if number.to_string() != value {
        return Err(Error::InvalidFormat);
    }
    Ok(number)
}

fn hash_part(hash: &mut Sha256, bytes: &[u8]) -> Result<(), Error> {
    let length = u64::try_from(bytes.len()).map_err(|_| Error::InvalidFormat)?;
    hash.update(length.to_be_bytes());
    hash.update(bytes);
    Ok(())
}

pub(crate) async fn baseline_digest(
    connection: &mut SqliteConnection,
    floor: u64,
) -> Result<(i64, String), Error> {
    let mut rows = sqlx::query(
        "SELECT node_id,seq,post_state FROM journal_baseline ORDER BY node_id COLLATE BINARY",
    )
    .fetch(connection);
    let mut count = 0_i64;
    let mut hash = Sha256::new();
    hash_part(&mut hash, b"notryx-journal-baseline-v1")?;
    hash_part(&mut hash, floor.to_string().as_bytes())?;
    while let Some(row) = rows.try_next().await.map_err(state::db_error)? {
        count = count.checked_add(1).ok_or(Error::InvalidFormat)?;
        let id: String = row.try_get("node_id").map_err(state::db_error)?;
        let seq: String = row.try_get("seq").map_err(state::db_error)?;
        let post: String = row.try_get("post_state").map_err(state::db_error)?;
        hash_part(&mut hash, id.as_bytes())?;
        hash_part(&mut hash, seq.as_bytes())?;
        hash_part(&mut hash, post.as_bytes())?;
    }
    let digest: String = hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Ok((count, digest))
}

pub(crate) async fn logical_bytes(connection: &mut SqliteConnection) -> Result<u64, Error> {
    let bytes: i64 = sqlx::query_scalar("SELECT \
        (SELECT COALESCE(SUM(length(CAST(seq AS BLOB)) + COALESCE(length(CAST(operation_id AS BLOB)),0) + CASE WHEN committed_at IS NULL THEN 0 ELSE 8 END),0) FROM change_sets) + \
        (SELECT COALESCE(SUM(length(CAST(seq AS BLOB)) + length(CAST(node_id AS BLOB)) + length(CAST(post_state AS BLOB))),0) FROM node_changes) + \
        (SELECT COALESCE(SUM(length(CAST(node_id AS BLOB)) + length(CAST(seq AS BLOB)) + length(CAST(post_state AS BLOB))),0) FROM journal_baseline)")
        .fetch_one(connection).await.map_err(state::db_error)?;
    u64::try_from(bytes).map_err(|_| Error::InvalidFormat)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RetentionReport {
    pub(crate) before_floor: u64,
    pub(crate) after_floor: u64,
    pub(crate) logical_bytes: u64,
    pub(crate) budget_bytes: u64,
    pub(crate) budget_exceeded: bool,
    pub(crate) expired_count: u64,
    pub(crate) budget_forced_count: u64,
}

impl Workspace {
    pub async fn retain_journal(
        &mut self,
        retention_seconds: u64,
        budget_bytes: u64,
        now_seconds: i64,
    ) -> Result<(), Error> {
        self.retain_journal_report(retention_seconds, budget_bytes, now_seconds)
            .await
            .map(|_| ())
    }

    pub(crate) async fn retain_journal_report(
        &mut self,
        retention_seconds: u64,
        budget_bytes: u64,
        now_seconds: i64,
    ) -> Result<RetentionReport, Error> {
        #[cfg(test)]
        let result = {
            self.retain_journal_inner(retention_seconds, budget_bytes, now_seconds, None)
                .await
        };
        #[cfg(not(test))]
        let result = {
            self.retain_journal_inner(retention_seconds, budget_bytes, now_seconds)
                .await
        };
        if matches!(&result, Err(Error::InvalidFormat)) {
            self.journal_integrity_failed = true;
        }
        result
    }

    #[cfg(test)]
    pub(crate) async fn retain_journal_test(
        &mut self,
        retention_seconds: u64,
        budget_bytes: u64,
        now_seconds: i64,
        before_commit: Option<crate::runtime::journal_retention_test_support::RetentionTestPause>,
    ) -> Result<RetentionReport, Error> {
        let result = self
            .retain_journal_inner(retention_seconds, budget_bytes, now_seconds, before_commit)
            .await;
        if matches!(&result, Err(Error::InvalidFormat)) {
            self.journal_integrity_failed = true;
        }
        result
    }

    async fn retain_journal_inner(
        &mut self,
        retention_seconds: u64,
        budget_bytes: u64,
        now_seconds: i64,
        #[cfg(test)] before_commit: Option<
            crate::runtime::journal_retention_test_support::RetentionTestPause,
        >,
    ) -> Result<RetentionReport, Error> {
        if !self.ready() {
            return Err(Error::RecoveryRequired);
        }
        self.verify_mutation_root()?;
        if now_seconds < 0 {
            return Err(Error::InvalidRequest);
        }
        let observed_at = now_seconds.checked_add(1).ok_or(Error::InvalidRequest)?;
        let retention = i64::try_from(retention_seconds).map_err(|_| Error::InvalidRequest)?;
        let root = self.state.root_id;
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
        let head = canonical_u64(row.try_get("head_seq").map_err(state::db_error)?)?;
        let floor = canonical_u64(row.try_get("journal_floor_seq").map_err(state::db_error)?)?;
        if floor > head {
            return Err(Error::InvalidFormat);
        }
        let actual_head = state::validate_journal(&mut tx, state::CURRENT_SCHEMA_VERSION).await?;
        if actual_head != head {
            return Err(Error::InvalidFormat);
        }
        state::validate_retention(&mut tx, root).await?;
        state::node::validate_tree(&mut tx, root, head, state::CURRENT_SCHEMA_VERSION).await?;

        let rows = sqlx::query("SELECT seq,committed_at,length(CAST(seq AS BLOB)) + COALESCE(length(CAST(operation_id AS BLOB)),0) AS stored_bytes FROM change_sets")
            .fetch_all(&mut *tx)
            .await
            .map_err(state::db_error)?;
        let mut changes = Vec::with_capacity(rows.len());
        for row in rows {
            let seq = canonical_u64(row.try_get("seq").map_err(state::db_error)?)?;
            let timestamp: Option<i64> = row.try_get("committed_at").map_err(state::db_error)?;
            let stored_bytes: i64 = row.try_get("stored_bytes").map_err(state::db_error)?;
            let stored_bytes = u64::try_from(stored_bytes).map_err(|_| Error::InvalidFormat)?;
            if timestamp.is_some_and(|value| value < 0) {
                return Err(Error::InvalidFormat);
            }
            changes.push((seq, timestamp, stored_bytes));
        }
        changes.sort_unstable_by_key(|(seq, _, _)| *seq);
        let mut previous = floor;
        for (seq, _, _) in &changes {
            if previous.checked_add(1) != Some(*seq) || *seq > head {
                return Err(Error::InvalidFormat);
            }
            previous = *seq;
        }
        if previous != head {
            return Err(Error::InvalidFormat);
        }
        sqlx::query("UPDATE change_sets SET committed_at=? WHERE committed_at IS NULL")
            .bind(observed_at)
            .execute(&mut *tx)
            .await
            .map_err(state::db_error)?;
        let mut new_floor = floor;
        let mut bytes = logical_bytes(&mut tx).await?;
        let mut expired_count = 0_u64;
        let mut budget_forced_count = 0_u64;
        for (seq, timestamp, stored_bytes) in changes {
            let expired = timestamp
                .and_then(|time| time.checked_add(retention))
                .is_some_and(|expiry| expiry <= now_seconds);
            if !expired && bytes <= budget_bytes {
                break;
            }
            if expired {
                expired_count = expired_count.checked_add(1).ok_or(Error::InvalidFormat)?;
            } else {
                budget_forced_count = budget_forced_count
                    .checked_add(1)
                    .ok_or(Error::InvalidFormat)?;
            }
            let seq_text = seq.to_string();
            let mut removed_bytes = stored_bytes.checked_add(8).ok_or(Error::InvalidFormat)?;
            let posts = sqlx::query("SELECT node_id,post_state,length(CAST(seq AS BLOB)) + length(CAST(node_id AS BLOB)) + length(CAST(post_state AS BLOB)) AS stored_bytes FROM node_changes WHERE seq=?")
                .bind(&seq_text)
                .fetch_all(&mut *tx)
                .await
                .map_err(state::db_error)?;
            if posts.is_empty() {
                return Err(Error::InvalidFormat);
            }
            for post in posts {
                let id_text: String = post.try_get("node_id").map_err(state::db_error)?;
                let id: NodeId = id_text.parse().map_err(|_| Error::InvalidFormat)?;
                if id.to_string() != id_text {
                    return Err(Error::InvalidFormat);
                }
                let text: String = post.try_get("post_state").map_err(state::db_error)?;
                state::node::journal_snapshot(&text, id, ChangeSeq::new(seq))?;
                let post_bytes: i64 = post.try_get("stored_bytes").map_err(state::db_error)?;
                let post_bytes = u64::try_from(post_bytes).map_err(|_| Error::InvalidFormat)?;
                removed_bytes = removed_bytes
                    .checked_add(post_bytes)
                    .ok_or(Error::InvalidFormat)?;
                let old: Option<i64> =
                    sqlx::query_scalar("SELECT length(CAST(node_id AS BLOB)) + length(CAST(seq AS BLOB)) + length(CAST(post_state AS BLOB)) FROM journal_baseline WHERE node_id=?")
                        .bind(&id_text)
                        .fetch_optional(&mut *tx)
                        .await
                        .map_err(state::db_error)?;
                if let Some(old_bytes) = old {
                    bytes = bytes
                        .checked_sub(u64::try_from(old_bytes).map_err(|_| Error::InvalidFormat)?)
                        .ok_or(Error::InvalidFormat)?;
                }
                bytes = bytes.checked_add(post_bytes).ok_or(Error::InvalidFormat)?;
                sqlx::query("INSERT INTO journal_baseline(node_id,seq,post_state) VALUES(?,?,?) ON CONFLICT(node_id) DO UPDATE SET seq=excluded.seq,post_state=excluded.post_state")
                    .bind(&id_text).bind(&seq_text).bind(&text)
                    .execute(&mut *tx).await.map_err(state::db_error)?;
            }
            sqlx::query("DELETE FROM node_changes WHERE seq=?")
                .bind(&seq_text)
                .execute(&mut *tx)
                .await
                .map_err(state::db_error)?;
            let deleted = sqlx::query("DELETE FROM change_sets WHERE seq=?")
                .bind(&seq_text)
                .execute(&mut *tx)
                .await
                .map_err(state::db_error)?
                .rows_affected();
            if deleted != 1 {
                return Err(Error::InvalidFormat);
            }
            new_floor = seq;
            bytes = bytes
                .checked_sub(removed_bytes)
                .ok_or(Error::InvalidFormat)?;
        }
        if new_floor != floor {
            let (count, digest) = baseline_digest(&mut tx, new_floor).await?;
            let updated = sqlx::query("UPDATE workspaces SET journal_floor_seq=?,journal_baseline_count=?,journal_baseline_hash=? WHERE singleton=1 AND journal_floor_seq=? AND head_seq=? AND epoch=?")
                .bind(new_floor.to_string()).bind(count).bind(digest)
                .bind(floor.to_string()).bind(head.to_string()).bind(epoch)
                .execute(&mut *tx).await.map_err(state::db_error)?.rows_affected();
            if updated != 1 {
                return Err(Error::InvalidFormat);
            }
        }
        state::validate_retention(&mut tx, root).await?;
        if state::validate_journal(&mut tx, state::CURRENT_SCHEMA_VERSION).await? != head {
            return Err(Error::InvalidFormat);
        }
        state::node::validate_tree(&mut tx, root, head, state::CURRENT_SCHEMA_VERSION).await?;
        #[cfg(test)]
        if let Some(pause) = before_commit {
            if pause.stage
                != crate::runtime::journal_retention_test_support::RetentionPauseStage::BeforeCommit
            {
                return Err(Error::InvalidRequest);
            }
            pause.wait().await?;
        }
        tx.commit().await.map_err(state::db_error)?;
        Ok(RetentionReport {
            before_floor: floor,
            after_floor: new_floor,
            logical_bytes: bytes,
            budget_bytes,
            budget_exceeded: bytes > budget_bytes,
            expired_count,
            budget_forced_count,
        })
    }
}
