use super::{PendingIntent, State, canonical_u64, db_error, decode, encode, node, parse};
use crate::{Error, NodeSnapshot, OperationId, Outcome, RecoveryDetail, RecoverySummary};
use sqlx::{Connection, Row, SqliteConnection};
use std::collections::{BTreeSet, HashSet};

pub(super) async fn initialize(connection: &mut SqliteConnection) -> Result<(), Error> {
    sqlx::raw_sql(
        "CREATE TABLE recovery_cases (
        operation_id TEXT PRIMARY KEY NOT NULL, intent TEXT NOT NULL, paths TEXT NOT NULL,
        resolved INTEGER NOT NULL DEFAULT 0 CHECK(resolved IN(0,1)), resolved_head TEXT);
        CREATE TABLE recovery_objects (
        operation_id TEXT NOT NULL REFERENCES recovery_cases(operation_id) ON DELETE CASCADE,
        hash TEXT NOT NULL, PRIMARY KEY(operation_id,hash));",
    )
    .execute(connection)
    .await
    .map_err(db_error)?;
    Ok(())
}

fn intent_hashes(intent: &PendingIntent) -> BTreeSet<String> {
    match intent {
        PendingIntent::Save(i) => [
            Some(i.before.content_hash.clone()),
            Some(i.proposed.content_hash.clone()),
        ]
        .into_iter()
        .flatten()
        .collect(),
        PendingIntent::Create(i) => i.created.content_hash.iter().cloned().collect(),
        PendingIntent::Move(i) => i
            .before
            .content_hash
            .iter()
            .chain(i.proposed.content_hash.iter())
            .chain(i.observed_hashes.iter())
            .cloned()
            .collect(),
        PendingIntent::Lifecycle(i) => i
            .before
            .content_hash
            .iter()
            .chain(i.proposed.content_hash.iter())
            .cloned()
            .collect(),
    }
}

fn same_request(left: &PendingIntent, right: &PendingIntent) -> Result<bool, Error> {
    let mut left = left.clone();
    let mut right = right.clone();
    for intent in [&mut left, &mut right] {
        if let PendingIntent::Move(intent) = intent {
            intent.observed_hashes.clear();
        }
    }
    Ok(encode(&left)? == encode(&right)?)
}

async fn intent_paths(
    connection: &mut SqliteConnection,
    intent: &PendingIntent,
) -> Result<Vec<String>, Error> {
    Ok(match intent {
        PendingIntent::Save(i) => {
            let mut segments = vec![i.before.name.clone()];
            let mut current = Some(i.before.parent_id);
            let mut seen = HashSet::new();
            while let Some(id) = current {
                if !seen.insert(id) {
                    return Err(Error::InvalidFormat);
                }
                let row = sqlx::query("SELECT * FROM nodes WHERE node_id=?")
                    .bind(id.to_string())
                    .fetch_one(&mut *connection)
                    .await
                    .map_err(db_error)?;
                let parent = node::snapshot(&row)?;
                current = parent.parent_id;
                if current.is_some() {
                    segments.push(parent.name);
                }
            }
            segments.reverse();
            vec![segments.join("/")]
        }
        PendingIntent::Create(i) => vec![i.path.clone()],
        PendingIntent::Move(i) => vec![i.old_path.clone(), i.new_path.clone()],
        PendingIntent::Lifecycle(i) => vec![i.path.clone()],
    })
}

async fn retain_case(
    connection: &mut SqliteConnection,
    intent: &PendingIntent,
    paths: &[String],
    observed: Option<&str>,
) -> Result<(), Error> {
    let id = intent.operation_id().to_string();
    sqlx::query("INSERT INTO recovery_cases(operation_id,intent,paths) VALUES(?,?,?) ON CONFLICT(operation_id) DO NOTHING")
        .bind(&id).bind(encode(intent)?).bind(encode(&paths)?).execute(&mut *connection).await.map_err(db_error)?;
    let mut hashes = intent_hashes(intent);
    if let Some(hash) = observed {
        hashes.insert(hash.to_owned());
    }
    for hash in hashes {
        sqlx::query("INSERT INTO recovery_objects VALUES(?,?) ON CONFLICT DO NOTHING")
            .bind(&id)
            .bind(hash)
            .execute(&mut *connection)
            .await
            .map_err(db_error)?;
    }
    Ok(())
}

pub(super) async fn migrate_existing(connection: &mut SqliteConnection) -> Result<(), Error> {
    let rows =
        sqlx::query("SELECT intent,observed_hash FROM write_intents WHERE recovery_conflict=1")
            .fetch_all(&mut *connection)
            .await
            .map_err(db_error)?;
    for row in rows {
        let intent: PendingIntent = decode(row.try_get("intent").map_err(db_error)?)?;
        let paths = intent_paths(connection, &intent).await?;
        retain_case(
            connection,
            &intent,
            &paths,
            row.try_get("observed_hash").map_err(db_error)?,
        )
        .await?;
    }
    Ok(())
}

pub(super) async fn validate(connection: &mut SqliteConnection) -> Result<(), Error> {
    let head: String = sqlx::query_scalar("SELECT head_seq FROM workspaces WHERE singleton=1")
        .fetch_one(&mut *connection)
        .await
        .map_err(db_error)?;
    let current_head = canonical_u64(&head)?;
    let rows = sqlx::query("SELECT * FROM recovery_cases")
        .fetch_all(&mut *connection)
        .await
        .map_err(db_error)?;
    for row in rows {
        let id: &str = row.try_get("operation_id").map_err(db_error)?;
        let parsed: OperationId = parse(id)?;
        let intent: PendingIntent = decode(row.try_get("intent").map_err(db_error)?)?;
        let paths: Vec<String> = decode(row.try_get("paths").map_err(db_error)?)?;
        if parsed.to_string() != id
            || intent.operation_id() != parsed
            || paths.is_empty()
            || paths.len() > 2
        {
            return Err(Error::InvalidFormat);
        }
        for path in &paths {
            if path.len() > 1024
                || path
                    .split('/')
                    .any(|part| notryx_domain::mutation::validate_name(part).is_err())
            {
                return Err(Error::InvalidFormat);
            }
        }
        let resolved: i64 = row.try_get("resolved").map_err(db_error)?;
        if resolved == 0 || !matches!(intent, PendingIntent::Save(_)) {
            if paths != intent_paths(connection, &intent).await? {
                return Err(Error::InvalidFormat);
            }
        } else if let PendingIntent::Save(intent) = &intent
            && (paths.len() != 1
                || paths[0].rsplit('/').next() != Some(intent.before.name.as_str()))
        {
            return Err(Error::InvalidFormat);
        }
        let head: Option<&str> = row.try_get("resolved_head").map_err(db_error)?;
        let pending: Option<i64> =
            sqlx::query_scalar("SELECT recovery_conflict FROM write_intents WHERE operation_id=?")
                .bind(id)
                .fetch_optional(&mut *connection)
                .await
                .map_err(db_error)?;
        match (resolved, head, pending) {
            (0, None, Some(1)) => {
                let current: String =
                    sqlx::query_scalar("SELECT intent FROM write_intents WHERE operation_id=?")
                        .bind(id)
                        .fetch_one(&mut *connection)
                        .await
                        .map_err(db_error)?;
                if !same_request(&intent, &decode(&current)?)? {
                    return Err(Error::InvalidFormat);
                }
            }
            (1, Some(head), None) => {
                if canonical_u64(head)? > current_head {
                    return Err(Error::InvalidFormat);
                }
                let receipt: Option<String> = sqlx::query_scalar(
                    "SELECT outcome FROM operation_receipts WHERE operation_id=? AND pending=0",
                )
                .bind(id)
                .fetch_optional(&mut *connection)
                .await
                .map_err(db_error)?;
                if let Some(outcome) = receipt {
                    if decode::<Outcome>(&outcome)? != Outcome::RecoveryConflict {
                        return Err(Error::InvalidFormat);
                    }
                } else {
                    let retired: bool = sqlx::query_scalar(
                        "SELECT EXISTS(SELECT 1 FROM retired_operations WHERE operation_id=?)",
                    )
                    .bind(id)
                    .fetch_one(&mut *connection)
                    .await
                    .map_err(db_error)?;
                    if !retired {
                        return Err(Error::InvalidFormat);
                    }
                }
            }
            _ => return Err(Error::InvalidFormat),
        }
        let hashes: Vec<String> =
            sqlx::query_scalar("SELECT hash FROM recovery_objects WHERE operation_id=?")
                .bind(id)
                .fetch_all(&mut *connection)
                .await
                .map_err(db_error)?;
        if hashes.iter().any(|h| {
            h.len() != 64
                || !h
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        }) || !intent_hashes(&intent)
            .iter()
            .all(|hash| hashes.contains(hash))
        {
            return Err(Error::InvalidFormat);
        }
    }
    let missing: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM write_intents i LEFT JOIN recovery_cases c USING(operation_id) WHERE i.recovery_conflict=1 AND (c.operation_id IS NULL OR c.resolved<>0))")
        .fetch_one(connection).await.map_err(db_error)?;
    if missing {
        return Err(Error::InvalidFormat);
    }
    Ok(())
}

impl State {
    pub async fn quarantine_intent(
        &mut self,
        id: OperationId,
        observed: Option<&str>,
        updated: Option<&PendingIntent>,
    ) -> Result<(), Error> {
        let row =
            sqlx::query("SELECT intent,observed_hash FROM write_intents WHERE operation_id=?")
                .bind(id.to_string())
                .fetch_one(&mut self.connection)
                .await
                .map_err(db_error)?;
        let original: PendingIntent = decode(row.try_get("intent").map_err(db_error)?)?;
        let intent = updated.unwrap_or(&original);
        if intent.operation_id() != id {
            return Err(Error::InvalidFormat);
        }
        let paths = intent_paths(&mut self.connection, intent).await?;
        let mut tx = self.connection.begin().await.map_err(db_error)?;
        retain_case(&mut tx, intent, &paths, observed).await?;
        retain_case(
            &mut tx,
            &original,
            &paths,
            row.try_get("observed_hash").map_err(db_error)?,
        )
        .await?;
        sqlx::query("UPDATE write_intents SET intent=?,observed_hash=COALESCE(?,observed_hash),recovery_conflict=1 WHERE operation_id=?")
            .bind(encode(intent)?).bind(observed).bind(id.to_string()).execute(&mut *tx).await.map_err(db_error)?;
        tx.commit().await.map_err(db_error)
    }

    pub async fn recovery_list(
        &mut self,
        after: Option<OperationId>,
        limit: u16,
    ) -> Result<Vec<RecoverySummary>, Error> {
        if !(1..=101).contains(&limit) {
            return Err(Error::InvalidRequest);
        }
        let rows = sqlx::query("SELECT c.operation_id,c.resolved,COUNT(o.hash) AS objects FROM recovery_cases c LEFT JOIN recovery_objects o USING(operation_id) WHERE c.operation_id COLLATE BINARY > ? GROUP BY c.operation_id ORDER BY c.operation_id COLLATE BINARY LIMIT ?")
            .bind(after.map(|id|id.to_string()).unwrap_or_default()).bind(i64::from(limit)).fetch_all(&mut self.connection).await.map_err(db_error)?;
        rows.iter()
            .map(|row| {
                Ok(RecoverySummary {
                    operation_id: parse(row.try_get("operation_id").map_err(db_error)?)?,
                    resolved: row.try_get::<i64, _>("resolved").map_err(db_error)? == 1,
                    retained_objects: row
                        .try_get::<i64, _>("objects")
                        .map_err(db_error)?
                        .try_into()
                        .map_err(|_| Error::InvalidFormat)?,
                })
            })
            .collect()
    }

    pub async fn recovery_detail(&mut self, id: OperationId) -> Result<RecoveryDetail, Error> {
        self.recovery_detail_page(id, None).await
    }

    pub async fn recovery_detail_page(
        &mut self,
        id: OperationId,
        after_hash: Option<&str>,
    ) -> Result<RecoveryDetail, Error> {
        if after_hash.is_some_and(|hash| {
            hash.len() != 64
                || !hash
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        }) {
            return Err(Error::InvalidRequest);
        }
        let row = sqlx::query("SELECT paths,resolved FROM recovery_cases WHERE operation_id=?")
            .bind(id.to_string())
            .fetch_optional(&mut self.connection)
            .await
            .map_err(db_error)?
            .ok_or(Error::NotFound)?;
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM recovery_objects WHERE operation_id=?")
                .bind(id.to_string())
                .fetch_one(&mut self.connection)
                .await
                .map_err(db_error)?;
        let mut objects: Vec<String> = sqlx::query_scalar("SELECT hash FROM recovery_objects WHERE operation_id=? AND hash>? ORDER BY hash LIMIT 65")
            .bind(id.to_string()).bind(after_hash.unwrap_or("")).fetch_all(&mut self.connection).await.map_err(db_error)?;
        let next_after_hash = if objects.len() > 64 {
            objects.pop();
            objects.last().cloned()
        } else {
            None
        };
        Ok(RecoveryDetail {
            summary: RecoverySummary {
                operation_id: id,
                resolved: row.try_get::<i64, _>("resolved").map_err(db_error)? == 1,
                retained_objects: count.try_into().map_err(|_| Error::InvalidFormat)?,
            },
            paths: decode(row.try_get("paths").map_err(db_error)?)?,
            objects,
            next_after_hash,
        })
    }

    pub async fn recovery_hashes(&mut self) -> Result<Vec<String>, Error> {
        sqlx::query_scalar("SELECT DISTINCT hash FROM recovery_objects")
            .fetch_all(&mut self.connection)
            .await
            .map_err(db_error)
    }

    pub async fn retain_resolution_objects(
        &mut self,
        id: OperationId,
        hashes: &BTreeSet<String>,
    ) -> Result<(), Error> {
        let mut tx = self.connection.begin().await.map_err(db_error)?;
        for hash in hashes {
            sqlx::query("INSERT INTO recovery_objects VALUES(?,?) ON CONFLICT DO NOTHING")
                .bind(id.to_string())
                .bind(hash)
                .execute(&mut *tx)
                .await
                .map_err(db_error)?;
        }
        tx.commit().await.map_err(db_error)
    }

    pub async fn finish_resolution(
        &mut self,
        id: OperationId,
        changes: &[Vec<NodeSnapshot>],
    ) -> Result<(), Error> {
        let mut tx = self.connection.begin().await.map_err(db_error)?;
        for posts in changes {
            let seq = posts
                .first()
                .ok_or(Error::InvalidFormat)?
                .last_change_seq
                .get();
            sqlx::query("INSERT INTO change_sets(seq,operation_id) VALUES(?,NULL)")
                .bind(seq.to_string())
                .execute(&mut *tx)
                .await
                .map_err(db_error)?;
            for node in posts {
                if node.last_change_seq.get() != seq {
                    return Err(Error::InvalidFormat);
                }
                write_node(&mut tx, node).await?;
                sqlx::query("INSERT INTO node_changes VALUES(?,?,?)")
                    .bind(seq.to_string())
                    .bind(node.node_id.to_string())
                    .bind(encode(&node::node_post_state(node))?)
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
                return Err(Error::InvalidFormat);
            }
        }
        let changed = sqlx::query("UPDATE operation_receipts SET outcome=?,node_etag=NULL,pending=0 WHERE operation_id=? AND pending=1")
            .bind(encode(&Outcome::RecoveryConflict)?).bind(id.to_string()).execute(&mut *tx).await.map_err(db_error)?.rows_affected();
        if changed != 1 {
            return Err(Error::InvalidFormat);
        }
        let changed =
            sqlx::query("DELETE FROM write_intents WHERE operation_id=? AND recovery_conflict=1")
                .bind(id.to_string())
                .execute(&mut *tx)
                .await
                .map_err(db_error)?
                .rows_affected();
        if changed != 1 {
            return Err(Error::InvalidFormat);
        }
        let changed = sqlx::query("UPDATE recovery_cases SET resolved=1,resolved_head=(SELECT head_seq FROM workspaces WHERE singleton=1) WHERE operation_id=? AND resolved=0")
            .bind(id.to_string()).execute(&mut *tx).await.map_err(db_error)?.rows_affected();
        if changed != 1 {
            return Err(Error::InvalidFormat);
        }
        super::validate_layout(&mut tx, self.root_id, super::CURRENT_SCHEMA_VERSION).await?;
        tx.commit().await.map_err(db_error)
    }

    pub async fn release_recovery(&mut self, id: OperationId) -> Result<(), Error> {
        if !self.recovery_detail(id).await?.summary.resolved {
            return Err(Error::OperationPending);
        }
        sqlx::query("DELETE FROM recovery_cases WHERE operation_id=? AND resolved=1")
            .bind(id.to_string())
            .execute(&mut self.connection)
            .await
            .map_err(db_error)?;
        Ok(())
    }

    pub async fn validate_current(&mut self) -> Result<(), Error> {
        super::validate_layout(
            &mut self.connection,
            self.root_id,
            super::CURRENT_SCHEMA_VERSION,
        )
        .await
    }
}

async fn write_node(connection: &mut SqliteConnection, node: &NodeSnapshot) -> Result<(), Error> {
    sqlx::query("INSERT INTO nodes(node_id,parent_id,name,normalized_name_key,kind,content_rev,meta_rev,content_hash,size,children_generation,last_change_seq,deleted) VALUES(?,?,?,?,?,?,?,?,?,?,?,?) ON CONFLICT(node_id) DO UPDATE SET parent_id=excluded.parent_id,name=excluded.name,normalized_name_key=excluded.normalized_name_key,kind=excluded.kind,content_rev=excluded.content_rev,meta_rev=excluded.meta_rev,content_hash=excluded.content_hash,size=excluded.size,children_generation=excluded.children_generation,last_change_seq=excluded.last_change_seq,deleted=excluded.deleted")
        .bind(node.node_id.to_string()).bind(node.parent_id.map(|id|id.to_string())).bind(&node.name)
        .bind(notryx_domain::mutation::normalized_name_key(&node.name)).bind(match node.kind { notryx_protocol::NodeKind::File => "file", notryx_protocol::NodeKind::Folder => "folder" })
        .bind(node.content_rev.as_ref().map(|r|r.as_str())).bind(node.meta_rev.as_str()).bind(&node.content_hash).bind(node.size.map(|s|s.to_string()))
        .bind(node.children_generation.to_string()).bind(node.last_change_seq.get().to_string()).bind(node.deleted)
        .execute(connection).await.map_err(db_error)?;
    Ok(())
}
