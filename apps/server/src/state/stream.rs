use super::{State, canonical_u64, db_error, decode, encode, parse};
use crate::{
    AbandonOperation, Error, OperationRecord, OperationStreamState, Outcome, Receipt, SaveContent,
};
use notryx_domain::OperationId;
use sqlx::{Connection, Row, Sqlite, SqliteConnection, Transaction};
use uuid::{Uuid, Variant};

pub(super) async fn initialize(connection: &mut SqliteConnection) -> Result<(), Error> {
    sqlx::raw_sql(
        "ALTER TABLE operation_streams ADD COLUMN retired_through TEXT NOT NULL DEFAULT '0';
         CREATE TABLE retired_operations (
             operation_id TEXT PRIMARY KEY NOT NULL,
             stream_id TEXT NOT NULL REFERENCES operation_streams(stream_id),
             operation_seq TEXT NOT NULL,
             UNIQUE(stream_id,operation_seq));
         PRAGMA user_version=3;",
    )
    .execute(connection)
    .await
    .map_err(db_error)?;
    Ok(())
}

pub(super) async fn migrate_receipt_etags(connection: &mut SqliteConnection) -> Result<(), Error> {
    sqlx::query("ALTER TABLE operation_receipts ADD COLUMN node_etag TEXT")
        .execute(&mut *connection)
        .await
        .map_err(db_error)?;
    let rows = sqlx::query("SELECT operation_id,outcome FROM operation_receipts")
        .fetch_all(&mut *connection)
        .await
        .map_err(db_error)?;
    for row in rows {
        let outcome: Outcome = decode(row.try_get("outcome").map_err(db_error)?)?;
        if let Some(etag) = crate::node::legacy_receipt_etag(&outcome)? {
            sqlx::query("UPDATE operation_receipts SET node_etag=? WHERE operation_id=?")
                .bind(etag)
                .bind(row.try_get::<&str, _>("operation_id").map_err(db_error)?)
                .execute(&mut *connection)
                .await
                .map_err(db_error)?;
        }
    }
    Ok(())
}

pub(super) async fn validate(connection: &mut SqliteConnection, version: i64) -> Result<(), Error> {
    if version >= 5 {
        sqlx::query("SELECT node_etag FROM operation_receipts LIMIT 0")
            .fetch_all(&mut *connection)
            .await
            .map_err(|_| Error::InvalidFormat)?;
    }
    let streams = sqlx::query("SELECT * FROM operation_streams")
        .fetch_all(&mut *connection)
        .await
        .map_err(db_error)?;
    if version >= 3 {
        let overlap: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM retired_operations r JOIN operation_receipts a USING(operation_id)")
            .fetch_one(&mut *connection).await.map_err(db_error)?;
        if overlap != 0 {
            return Err(Error::InvalidFormat);
        }
    }
    for stream in streams {
        let id: String = stream.try_get("stream_id").map_err(db_error)?;
        let parsed: Uuid = parse(&id)?;
        if parsed.get_version_num() != 4
            || parsed.get_variant() != Variant::RFC4122
            || parsed.to_string() != id
        {
            return Err(Error::InvalidFormat);
        }
        let last = canonical_u64(stream.try_get("last_seq").map_err(db_error)?)?;
        let retired = if version >= 3 {
            canonical_u64(stream.try_get("retired_through").map_err(db_error)?)?
        } else {
            0
        };
        if retired > last {
            return Err(Error::InvalidFormat);
        }
        let rows = sqlx::query("SELECT * FROM operation_receipts WHERE stream_id=?")
            .bind(&id)
            .fetch_all(&mut *connection)
            .await
            .map_err(db_error)?;
        let mut sequences = Vec::new();
        for row in rows {
            let operation: String = row.try_get("operation_id").map_err(db_error)?;
            if parse::<OperationId>(&operation)?.to_string() != operation {
                return Err(Error::InvalidFormat);
            }
            let sequence = canonical_u64(row.try_get("operation_seq").map_err(db_error)?)?;
            sequences.push(sequence);
            let outcome: Outcome = decode(row.try_get("outcome").map_err(db_error)?)?;
            let pending: i64 = row.try_get("pending").map_err(db_error)?;
            if (pending == 1) != (outcome == Outcome::Pending) || (pending == 1 && sequence != last)
            {
                return Err(Error::InvalidFormat);
            }
            if version >= 5 {
                crate::node::validate_receipt_etag(
                    &outcome,
                    row.try_get::<Option<&str>, _>("node_etag")
                        .map_err(db_error)?,
                )?;
            }
        }
        validate_range(&mut sequences, retired, last)?;
        if version >= 3 {
            let rows = sqlx::query(
                "SELECT operation_id,operation_seq FROM retired_operations WHERE stream_id=?",
            )
            .bind(&id)
            .fetch_all(&mut *connection)
            .await
            .map_err(db_error)?;
            let mut sequences = Vec::new();
            for row in rows {
                let operation: String = row.try_get("operation_id").map_err(db_error)?;
                if parse::<OperationId>(&operation)?.to_string() != operation {
                    return Err(Error::InvalidFormat);
                }
                sequences.push(canonical_u64(
                    row.try_get("operation_seq").map_err(db_error)?,
                )?);
            }
            validate_range(&mut sequences, 0, retired)?;
        }
    }
    Ok(())
}

fn validate_range(sequences: &mut [u64], after: u64, through: u64) -> Result<(), Error> {
    sequences.sort_unstable();
    if through.checked_sub(after) != Some(sequences.len() as u64)
        || sequences.iter().enumerate().any(|(index, value)| {
            after
                .checked_add(index as u64)
                .and_then(|value| value.checked_add(1))
                != Some(*value)
        })
    {
        return Err(Error::InvalidFormat);
    }
    Ok(())
}

async fn read_stream(
    connection: &mut SqliteConnection,
    epoch: notryx_domain::WorkspaceEpoch,
    stream_id: Uuid,
) -> Result<OperationStreamState, Error> {
    let row =
        sqlx::query("SELECT last_seq,retired_through FROM operation_streams WHERE stream_id=?")
            .bind(stream_id.to_string())
            .fetch_optional(connection)
            .await
            .map_err(db_error)?
            .ok_or(Error::UnknownStream)?;
    let state = OperationStreamState {
        workspace_epoch: epoch,
        stream_id,
        last_seq: canonical_u64(row.try_get("last_seq").map_err(db_error)?)?,
        retired_through: canonical_u64(row.try_get("retired_through").map_err(db_error)?)?,
    };
    if state.retired_through > state.last_seq {
        return Err(Error::InvalidFormat);
    }
    Ok(state)
}

impl State {
    pub async fn register_stream(&mut self) -> Result<Uuid, Error> {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO operation_streams(stream_id,last_seq) VALUES(?,'0')")
            .bind(id.to_string())
            .execute(&mut self.connection)
            .await
            .map_err(db_error)?;
        Ok(id)
    }

    pub async fn previous(
        &mut self,
        request: &SaveContent,
        fingerprint: &str,
    ) -> Result<Option<Receipt>, Error> {
        self.previous_identity(
            request.stream_id,
            request.operation_seq,
            request.operation_id,
            Some(fingerprint),
        )
        .await
    }

    pub async fn previous_identity(
        &mut self,
        stream_id: Uuid,
        sequence: u64,
        operation_id: OperationId,
        fingerprint: Option<&str>,
    ) -> Result<Option<Receipt>, Error> {
        let stream = read_stream(&mut self.connection, self.epoch, stream_id).await?;
        if sequence <= stream.retired_through {
            return Err(Error::OperationRetired);
        }
        let retired: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM retired_operations WHERE operation_id=?)",
        )
        .bind(operation_id.to_string())
        .fetch_one(&mut self.connection)
        .await
        .map_err(db_error)?;
        if retired {
            return Err(Error::IdempotencyMismatch);
        }
        let rows = sqlx::query("SELECT * FROM operation_receipts WHERE operation_id=? OR (stream_id=? AND operation_seq=?)")
            .bind(operation_id.to_string()).bind(stream_id.to_string()).bind(sequence.to_string())
            .fetch_all(&mut self.connection).await.map_err(db_error)?;
        if rows.is_empty() {
            return Ok(None);
        }
        if rows.len() != 1 {
            return Err(Error::IdempotencyMismatch);
        }
        let row = &rows[0];
        if row.try_get::<String, _>("operation_id").map_err(db_error)? != operation_id.to_string()
            || row.try_get::<String, _>("stream_id").map_err(db_error)? != stream_id.to_string()
            || canonical_u64(row.try_get("operation_seq").map_err(db_error)?)? != sequence
            || fingerprint.is_some_and(|expected| {
                row.try_get::<&str, _>("fingerprint").ok() != Some(expected)
            })
        {
            return Err(Error::IdempotencyMismatch);
        }
        Ok(Some(Receipt {
            operation_id,
            outcome: decode(row.try_get("outcome").map_err(db_error)?)?,
        }))
    }

    pub async fn validate_admission(
        &mut self,
        stream_id: Uuid,
        sequence: u64,
    ) -> Result<(), Error> {
        let stream = read_stream(&mut self.connection, self.epoch, stream_id).await?;
        if sequence <= stream.retired_through {
            return Err(Error::OperationRetired);
        }
        if stream.last_seq.checked_add(1) != Some(sequence) {
            return Err(Error::OperationGap);
        }
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM operation_receipts WHERE stream_id=?")
                .bind(stream_id.to_string())
                .fetch_one(&mut self.connection)
                .await
                .map_err(db_error)?;
        if count >= 10_000 {
            return Err(Error::ReceiptLimit);
        }
        Ok(())
    }

    pub async fn operation(
        &mut self,
        stream_id: Uuid,
        operation_id: OperationId,
    ) -> Result<OperationRecord, Error> {
        let row = sqlx::query("SELECT operation_seq,outcome,node_etag FROM operation_receipts WHERE operation_id=? AND stream_id=?")
            .bind(operation_id.to_string()).bind(stream_id.to_string()).fetch_optional(&mut self.connection).await.map_err(db_error)?;
        if let Some(row) = row {
            return Ok(OperationRecord {
                operation_id,
                operation_seq: canonical_u64(row.try_get("operation_seq").map_err(db_error)?)?,
                workspace_epoch: self.epoch,
                node_etag: row.try_get("node_etag").map_err(db_error)?,
                outcome: decode(row.try_get("outcome").map_err(db_error)?)?,
            });
        }
        let retired: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM retired_operations WHERE operation_id=? AND stream_id=?)",
        )
        .bind(operation_id.to_string())
        .bind(stream_id.to_string())
        .fetch_one(&mut self.connection)
        .await
        .map_err(db_error)?;
        Err(if retired {
            Error::OperationRetired
        } else {
            Error::NotFound
        })
    }

    pub async fn ack_operations(
        &mut self,
        stream_id: Uuid,
        through: u64,
        #[cfg(test)] hook: &mut Option<crate::FailureHook>,
    ) -> Result<OperationStreamState, Error> {
        let mut tx = self.connection.begin().await.map_err(db_error)?;
        let mut stream = read_stream(&mut tx, self.epoch, stream_id).await?;
        if through > stream.last_seq {
            return Err(Error::InvalidAck);
        }
        if through <= stream.retired_through {
            tx.commit().await.map_err(db_error)?;
            return Ok(stream);
        }
        let rows = sqlx::query("SELECT operation_id,operation_seq,outcome,pending FROM operation_receipts WHERE stream_id=?")
            .bind(stream_id.to_string()).fetch_all(&mut *tx).await.map_err(db_error)?;
        let mut selected = Vec::new();
        let mut sequences = Vec::new();
        for row in rows {
            let sequence = canonical_u64(row.try_get("operation_seq").map_err(db_error)?)?;
            if sequence <= stream.retired_through {
                return Err(Error::InvalidFormat);
            }
            if sequence > through {
                continue;
            }
            let pending: i64 = row.try_get("pending").map_err(db_error)?;
            let outcome: Outcome = decode(row.try_get("outcome").map_err(db_error)?)?;
            if pending != 0 || outcome == Outcome::Pending {
                return Err(Error::OperationPending);
            }
            let operation_id: String = row.try_get("operation_id").map_err(db_error)?;
            let operation_id: OperationId = parse(&operation_id)?;
            selected.push((operation_id, sequence));
            sequences.push(sequence);
        }
        validate_range(&mut sequences, stream.retired_through, through)?;
        let changed = sqlx::query("UPDATE operation_streams SET retired_through=? WHERE stream_id=? AND retired_through=? AND last_seq=?")
            .bind(through.to_string()).bind(stream_id.to_string()).bind(stream.retired_through.to_string()).bind(stream.last_seq.to_string())
            .execute(&mut *tx).await.map_err(db_error)?.rows_affected();
        if changed != 1 {
            return Err(Error::InvalidFormat);
        }
        for (operation, sequence) in selected {
            sqlx::query("INSERT INTO retired_operations VALUES(?,?,?)")
                .bind(operation.to_string())
                .bind(stream_id.to_string())
                .bind(sequence.to_string())
                .execute(&mut *tx)
                .await
                .map_err(db_error)?;
            let deleted = sqlx::query(
                "DELETE FROM operation_receipts WHERE operation_id=? AND stream_id=? AND pending=0",
            )
            .bind(operation.to_string())
            .bind(stream_id.to_string())
            .execute(&mut *tx)
            .await
            .map_err(db_error)?
            .rows_affected();
            if deleted != 1 {
                return Err(Error::InvalidFormat);
            }
        }
        #[cfg(test)]
        if let Some(hook) = hook.as_mut() {
            hook(crate::Phase::BeforeAckCommit)?;
        }
        tx.commit().await.map_err(db_error)?;
        #[cfg(test)]
        if let Some(hook) = hook.as_mut() {
            hook(crate::Phase::AfterAckCommit)?;
        }
        stream.retired_through = through;
        Ok(stream)
    }

    pub async fn abandon_operation(
        &mut self,
        request: &AbandonOperation,
        fingerprint: &str,
    ) -> Result<(), Error> {
        let mut tx = self.connection.begin().await.map_err(db_error)?;
        insert_receipt(
            &mut tx,
            request.stream_id,
            request.operation_seq,
            request.operation_id,
            fingerprint,
            &Outcome::Abandoned,
        )
        .await?;
        tx.commit().await.map_err(db_error)
    }
}

pub(super) async fn insert_receipt(
    connection: &mut SqliteConnection,
    stream_id: Uuid,
    sequence: u64,
    operation_id: OperationId,
    fingerprint: &str,
    outcome: &Outcome,
) -> Result<(), Error> {
    let previous = sequence.checked_sub(1).ok_or(Error::InvalidRequest)?;
    let changed =
        sqlx::query("UPDATE operation_streams SET last_seq=? WHERE stream_id=? AND last_seq=?")
            .bind(sequence.to_string())
            .bind(stream_id.to_string())
            .bind(previous.to_string())
            .execute(&mut *connection)
            .await
            .map_err(db_error)?
            .rows_affected();
    if changed != 1 {
        return Err(Error::OperationGap);
    }
    sqlx::query("INSERT INTO operation_receipts(operation_id,stream_id,operation_seq,fingerprint,outcome,pending,node_etag) VALUES(?,?,?,?,?,?,?)")
        .bind(operation_id.to_string())
        .bind(stream_id.to_string())
        .bind(sequence.to_string())
        .bind(fingerprint)
        .bind(encode(outcome)?)
        .bind(i64::from(*outcome == Outcome::Pending))
        .bind(crate::node::receipt_etag(outcome)?)
        .execute(connection)
        .await
        .map_err(db_error)?;
    Ok(())
}

pub(super) async fn finish_pending(
    transaction: &mut Transaction<'_, Sqlite>,
    operation_id: OperationId,
    outcome: &Outcome,
) -> Result<(), Error> {
    // The caller owns the transaction that also commits node and journal updates.
    let changed = sqlx::query(
        "UPDATE operation_receipts SET outcome=?,node_etag=?,pending=0 WHERE operation_id=? AND pending=1",
    )
    .bind(encode(outcome)?)
    .bind(crate::node::receipt_etag(outcome)?)
    .bind(operation_id.to_string())
    .execute(&mut **transaction)
    .await
    .map_err(db_error)?
    .rows_affected();
    if changed != 1 {
        return Err(Error::InvalidFormat);
    }
    sqlx::query("DELETE FROM write_intents WHERE operation_id=?")
        .bind(operation_id.to_string())
        .execute(&mut **transaction)
        .await
        .map_err(db_error)?;
    Ok(())
}
