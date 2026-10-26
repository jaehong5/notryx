use super::{Document, Error, Outcome, Receipt, SaveContent};
use notryx_domain::{ChangeSeq, NodeId, WorkspaceEpoch, WorkspaceId};
use serde::{Deserialize, Serialize};
use sqlx::{
    Connection, Row, SqliteConnection,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous},
};
use std::{fs::OpenOptions, os::unix::fs::OpenOptionsExt, path::Path, time::Duration};
use uuid::Uuid;

const APPLICATION_ID: i64 = 0x4e545253;

pub(crate) fn db_error(_: impl std::fmt::Debug) -> Error {
    Error::Storage
}
fn decode<T: serde::de::DeserializeOwned>(text: &str) -> Result<T, Error> {
    serde_json::from_str(text).map_err(|_| Error::InvalidFormat)
}
fn encode(value: &impl Serialize) -> Result<String, Error> {
    serde_json::to_string(value).map_err(db_error)
}
fn parse<T: std::str::FromStr>(value: &str) -> Result<T, Error> {
    value.parse().map_err(|_| Error::InvalidFormat)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Intent {
    pub operation_id: notryx_domain::OperationId,
    pub before: Document,
    pub proposed: Document,
}

pub(crate) struct State {
    pub connection: SqliteConnection,
    pub workspace_id: WorkspaceId,
    pub epoch: WorkspaceEpoch,
    pub root_id: NodeId,
}

impl State {
    pub async fn initialize(path: &Path, workspace_id: WorkspaceId) -> Result<Self, Error> {
        // Only explicit initialization may create a DB. Existing empty files are never adopted.
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .map_err(db_error)?;
        file.sync_all().map_err(db_error)?;
        let mut connection = Self::connect(path).await?;
        let epoch = parse(&Uuid::new_v4().to_string())?;
        let root_id = parse(&Uuid::now_v7().to_string())?;
        let mut tx = connection.begin().await.map_err(db_error)?;
        sqlx::raw_sql("CREATE TABLE workspaces (
            singleton INTEGER PRIMARY KEY CHECK(singleton=1), workspace_id TEXT NOT NULL,
            epoch TEXT NOT NULL, root_id TEXT NOT NULL, head_seq TEXT NOT NULL);
            CREATE TABLE nodes (
                node_id TEXT PRIMARY KEY NOT NULL, parent_id TEXT REFERENCES nodes(node_id),
                name TEXT NOT NULL, normalized_name_key TEXT NOT NULL, kind TEXT NOT NULL,
                content_rev TEXT, meta_rev TEXT NOT NULL, content_hash TEXT, size TEXT,
                children_generation TEXT NOT NULL, last_change_seq TEXT NOT NULL,
                UNIQUE(parent_id, normalized_name_key));
            CREATE TABLE change_sets (seq TEXT PRIMARY KEY NOT NULL, operation_id TEXT UNIQUE);
            CREATE TABLE node_changes (seq TEXT NOT NULL REFERENCES change_sets(seq),
                node_id TEXT NOT NULL REFERENCES nodes(node_id), post_state TEXT NOT NULL,
                PRIMARY KEY(seq,node_id));
            CREATE TABLE operation_streams (stream_id TEXT PRIMARY KEY NOT NULL, last_seq TEXT NOT NULL);
            CREATE TABLE operation_receipts (
                operation_id TEXT PRIMARY KEY NOT NULL, stream_id TEXT NOT NULL REFERENCES operation_streams(stream_id),
                operation_seq TEXT NOT NULL, fingerprint TEXT NOT NULL, outcome TEXT NOT NULL,
                pending INTEGER NOT NULL CHECK(pending IN(0,1)), UNIQUE(stream_id,operation_seq));
            CREATE TABLE write_intents (
                operation_id TEXT PRIMARY KEY NOT NULL REFERENCES operation_receipts(operation_id),
                intent TEXT NOT NULL, observed_hash TEXT, recovery_conflict INTEGER NOT NULL DEFAULT 0);
            PRAGMA user_version=1; PRAGMA application_id=1314148947;")
            .execute(&mut *tx).await.map_err(db_error)?;
        sqlx::query("INSERT INTO workspaces VALUES(1,?,?,?,'0')")
            .bind(workspace_id.to_string())
            .bind(format!("{epoch}"))
            .bind(format!("{root_id}"))
            .execute(&mut *tx)
            .await
            .map_err(db_error)?;
        sqlx::query("INSERT INTO nodes VALUES(?,NULL,'','','folder',NULL,?,NULL,NULL,'0','0')")
            .bind(format!("{root_id}"))
            .bind(Uuid::new_v4().to_string())
            .execute(&mut *tx)
            .await
            .map_err(db_error)?;
        tx.commit().await.map_err(db_error)?;
        Ok(Self {
            connection,
            workspace_id,
            epoch,
            root_id,
        })
    }

    async fn connect(path: &Path) -> Result<SqliteConnection, Error> {
        SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(path)
                .journal_mode(SqliteJournalMode::Wal)
                .synchronous(SqliteSynchronous::Full)
                .foreign_keys(true)
                .busy_timeout(Duration::from_secs(2)),
        )
        .await
        .map_err(db_error)
    }

    pub async fn open(path: &Path, expected_id: WorkspaceId) -> Result<Self, Error> {
        let mut probe = SqliteConnection::connect_with(
            &SqliteConnectOptions::new().filename(path).read_only(true),
        )
        .await
        .map_err(|_| Error::InvalidFormat)?;
        let app_id: i64 = sqlx::query_scalar("PRAGMA application_id")
            .fetch_one(&mut probe)
            .await
            .map_err(db_error)?;
        let version: i64 = sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(&mut probe)
            .await
            .map_err(db_error)?;
        let check: String = sqlx::query_scalar("PRAGMA quick_check")
            .fetch_one(&mut probe)
            .await
            .map_err(db_error)?;
        if app_id != APPLICATION_ID || version != 1 || check != "ok" {
            return Err(Error::InvalidFormat);
        }
        let row =
            sqlx::query("SELECT workspace_id,epoch,root_id FROM workspaces WHERE singleton=1")
                .fetch_one(&mut probe)
                .await
                .map_err(db_error)?;
        let workspace_id: WorkspaceId = parse(row.try_get("workspace_id").map_err(db_error)?)?;
        if workspace_id != expected_id {
            return Err(Error::WorkspaceMismatch);
        }
        let epoch = parse(row.try_get("epoch").map_err(db_error)?)?;
        let root_id = parse(row.try_get("root_id").map_err(db_error)?)?;
        if !sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(&mut probe)
            .await
            .map_err(db_error)?
            .is_empty()
        {
            return Err(Error::InvalidFormat);
        }
        let inconsistent: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM operation_receipts r LEFT JOIN write_intents i USING(operation_id) WHERE (r.pending=1 AND i.operation_id IS NULL) OR (r.pending=0 AND i.operation_id IS NOT NULL)")
            .fetch_one(&mut probe).await.map_err(db_error)?;
        if inconsistent != 0 {
            return Err(Error::InvalidFormat);
        }
        validate_layout(&mut probe, root_id).await?;
        probe.close().await.map_err(db_error)?;
        Ok(Self {
            connection: Self::connect(path).await?,
            workspace_id,
            epoch,
            root_id,
        })
    }

    pub async fn next_change_seq(&mut self) -> Result<ChangeSeq, Error> {
        let head: String = sqlx::query_scalar("SELECT head_seq FROM workspaces WHERE singleton=1")
            .fetch_one(&mut self.connection)
            .await
            .map_err(db_error)?;
        let head: u64 = parse(&head)?;
        Ok(ChangeSeq::new(
            head.checked_add(1).ok_or(Error::InvalidFormat)?,
        ))
    }

    pub async fn document(&mut self, node_id: NodeId) -> Result<Option<Document>, Error> {
        let row = sqlx::query("SELECT * FROM nodes WHERE node_id=? AND kind='file'")
            .bind(node_id.to_string())
            .fetch_optional(&mut self.connection)
            .await
            .map_err(db_error)?;
        row.map(|row| {
            Ok(Document {
                node_id,
                parent_id: parse(row.try_get("parent_id").map_err(db_error)?)?,
                name: row.try_get("name").map_err(db_error)?,
                content_rev: row
                    .try_get::<String, _>("content_rev")
                    .map_err(db_error)?
                    .try_into()
                    .map_err(|_| Error::InvalidFormat)?,
                meta_rev: row
                    .try_get::<String, _>("meta_rev")
                    .map_err(db_error)?
                    .try_into()
                    .map_err(|_| Error::InvalidFormat)?,
                content_hash: row.try_get("content_hash").map_err(db_error)?,
                size: parse(row.try_get("size").map_err(db_error)?)?,
                last_change_seq: ChangeSeq::new(parse(
                    row.try_get("last_change_seq").map_err(db_error)?,
                )?),
            })
        })
        .transpose()
    }

    pub async fn documents(&mut self) -> Result<Vec<Document>, Error> {
        let ids: Vec<String> = sqlx::query_scalar("SELECT node_id FROM nodes WHERE kind='file'")
            .fetch_all(&mut self.connection)
            .await
            .map_err(db_error)?;
        let mut docs = Vec::new();
        for id in ids {
            docs.push(
                self.document(parse(&id)?)
                    .await?
                    .ok_or(Error::InvalidFormat)?,
            );
        }
        Ok(docs)
    }

    pub async fn adopt(&mut self, document: &Document) -> Result<(), Error> {
        let mut tx = self.connection.begin().await.map_err(db_error)?;
        let seq = document.last_change_seq.get().to_string();
        sqlx::query("INSERT INTO nodes VALUES(?,?,?,?,'file',?,?,?,?,'0',?)")
            .bind(document.node_id.to_string())
            .bind(document.parent_id.to_string())
            .bind(&document.name)
            .bind(notryx_domain::mutation::normalized_name_key(&document.name))
            .bind(document.content_rev.as_str())
            .bind(document.meta_rev.as_str())
            .bind(&document.content_hash)
            .bind(document.size.to_string())
            .bind(&seq)
            .execute(&mut *tx)
            .await
            .map_err(db_error)?;
        let row = sqlx::query("SELECT meta_rev, children_generation FROM nodes WHERE node_id=? AND kind='folder' AND parent_id IS NULL")
            .bind(self.root_id.to_string()).fetch_one(&mut *tx).await.map_err(db_error)?;
        let generation: u64 = parse(row.try_get("children_generation").map_err(db_error)?)?;
        let generation = generation.checked_add(1).ok_or(Error::InvalidFormat)?;
        sqlx::query("UPDATE nodes SET children_generation=?,last_change_seq=? WHERE node_id=?")
            .bind(generation.to_string())
            .bind(&seq)
            .bind(self.root_id.to_string())
            .execute(&mut *tx)
            .await
            .map_err(db_error)?;
        sqlx::query("INSERT INTO change_sets VALUES(?,NULL)")
            .bind(&seq)
            .execute(&mut *tx)
            .await
            .map_err(db_error)?;
        sqlx::query("INSERT INTO node_changes VALUES(?,?,?)")
            .bind(&seq)
            .bind(document.node_id.to_string())
            .bind(document_post_state(document)?)
            .execute(&mut *tx)
            .await
            .map_err(db_error)?;
        let root = serde_json::json!({"node_id":self.root_id,"parent_id":null,"name":"","kind":"folder",
            "content_rev":null,"meta_rev":row.try_get::<String,_>("meta_rev").map_err(db_error)?,"content_hash":null,
            "size":null,"deleted":false,"children_generation":generation.to_string(),"last_change_seq":seq});
        sqlx::query("INSERT INTO node_changes VALUES(?,?,?)")
            .bind(&seq)
            .bind(self.root_id.to_string())
            .bind(encode(&root)?)
            .execute(&mut *tx)
            .await
            .map_err(db_error)?;
        sqlx::query("UPDATE workspaces SET head_seq=? WHERE singleton=1")
            .bind(&seq)
            .execute(&mut *tx)
            .await
            .map_err(db_error)?;
        tx.commit().await.map_err(db_error)
    }

    pub async fn register_stream(&mut self) -> Result<Uuid, Error> {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO operation_streams VALUES(?,'0')")
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
        let last: Option<String> =
            sqlx::query_scalar("SELECT last_seq FROM operation_streams WHERE stream_id=?")
                .bind(request.stream_id.to_string())
                .fetch_optional(&mut self.connection)
                .await
                .map_err(db_error)?;
        let last: u64 = parse(&last.ok_or(Error::UnknownStream)?)?;
        let rows = sqlx::query("SELECT * FROM operation_receipts WHERE operation_id=? OR (stream_id=? AND operation_seq=?)")
            .bind(request.operation_id.to_string()).bind(request.stream_id.to_string()).bind(request.operation_seq.to_string())
            .fetch_all(&mut self.connection).await.map_err(db_error)?;
        if !rows.is_empty() {
            if rows.len() != 1 {
                return Err(Error::IdempotencyMismatch);
            }
            let row = &rows[0];
            if row.try_get::<String, _>("operation_id").map_err(db_error)?
                != request.operation_id.to_string()
                || row.try_get::<String, _>("stream_id").map_err(db_error)?
                    != request.stream_id.to_string()
                || row
                    .try_get::<String, _>("operation_seq")
                    .map_err(db_error)?
                    != request.operation_seq.to_string()
                || row.try_get::<String, _>("fingerprint").map_err(db_error)? != fingerprint
            {
                return Err(Error::IdempotencyMismatch);
            }
            return Ok(Some(Receipt {
                operation_id: request.operation_id,
                outcome: decode(row.try_get("outcome").map_err(db_error)?)?,
            }));
        }
        if last.checked_add(1) != Some(request.operation_seq) {
            return Err(Error::OperationGap);
        }
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM operation_receipts WHERE stream_id=?")
                .bind(request.stream_id.to_string())
                .fetch_one(&mut self.connection)
                .await
                .map_err(db_error)?;
        if count >= 10_000 {
            return Err(Error::ReceiptLimit);
        }
        Ok(None)
    }

    pub async fn admit(
        &mut self,
        request: &SaveContent,
        fingerprint: &str,
        outcome: &Outcome,
        intent: Option<&Intent>,
    ) -> Result<(), Error> {
        let mut tx = self.connection.begin().await.map_err(db_error)?;
        let changed =
            sqlx::query("UPDATE operation_streams SET last_seq=? WHERE stream_id=? AND last_seq=?")
                .bind(request.operation_seq.to_string())
                .bind(request.stream_id.to_string())
                .bind((request.operation_seq - 1).to_string())
                .execute(&mut *tx)
                .await
                .map_err(db_error)?
                .rows_affected();
        if changed != 1 {
            return Err(Error::OperationGap);
        }
        sqlx::query("INSERT INTO operation_receipts VALUES(?,?,?,?,?,?)")
            .bind(request.operation_id.to_string())
            .bind(request.stream_id.to_string())
            .bind(request.operation_seq.to_string())
            .bind(fingerprint)
            .bind(encode(outcome)?)
            .bind(i64::from(intent.is_some()))
            .execute(&mut *tx)
            .await
            .map_err(db_error)?;
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

    pub async fn pending(&mut self) -> Result<Vec<Intent>, Error> {
        let rows: Vec<String> = sqlx::query_scalar("SELECT intent FROM write_intents")
            .fetch_all(&mut self.connection)
            .await
            .map_err(db_error)?;
        rows.into_iter().map(|s| decode(&s)).collect()
    }

    pub async fn quarantine(
        &mut self,
        intent: &Intent,
        observed_hash: Option<&str>,
    ) -> Result<(), Error> {
        sqlx::query(
            "UPDATE write_intents SET observed_hash=?,recovery_conflict=1 WHERE operation_id=?",
        )
        .bind(observed_hash)
        .bind(intent.operation_id.to_string())
        .execute(&mut self.connection)
        .await
        .map_err(db_error)?;
        Ok(())
    }

    pub async fn is_quarantined(&mut self, intent: &Intent) -> Result<bool, Error> {
        let flag: i64 =
            sqlx::query_scalar("SELECT recovery_conflict FROM write_intents WHERE operation_id=?")
                .bind(intent.operation_id.to_string())
                .fetch_one(&mut self.connection)
                .await
                .map_err(db_error)?;
        Ok(flag != 0)
    }

    pub async fn finish(&mut self, intent: &Intent, applied: bool) -> Result<Receipt, Error> {
        let mut tx = self.connection.begin().await.map_err(db_error)?;
        let outcome = if applied {
            let doc = &intent.proposed;
            let seq = doc.last_change_seq.get().to_string();
            let changed = sqlx::query("UPDATE nodes SET content_rev=?,content_hash=?,size=?,last_change_seq=? WHERE node_id=? AND content_rev=? AND meta_rev=?")
                .bind(doc.content_rev.as_str()).bind(&doc.content_hash).bind(doc.size.to_string()).bind(&seq)
                .bind(doc.node_id.to_string()).bind(intent.before.content_rev.as_str()).bind(intent.before.meta_rev.as_str())
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
            sqlx::query("INSERT INTO node_changes VALUES(?,?,?)")
                .bind(&seq)
                .bind(doc.node_id.to_string())
                .bind(document_post_state(doc)?)
                .execute(&mut *tx)
                .await
                .map_err(db_error)?;
            let changed =
                sqlx::query("UPDATE workspaces SET head_seq=? WHERE singleton=1 AND head_seq=?")
                    .bind(&seq)
                    .bind((doc.last_change_seq.get() - 1).to_string())
                    .execute(&mut *tx)
                    .await
                    .map_err(db_error)?
                    .rows_affected();
            if changed != 1 {
                return Err(Error::InvalidFormat);
            }
            Outcome::Saved(doc.clone())
        } else {
            Outcome::NotApplied
        };
        let changed = sqlx::query(
            "UPDATE operation_receipts SET outcome=?,pending=0 WHERE operation_id=? AND pending=1",
        )
        .bind(encode(&outcome)?)
        .bind(intent.operation_id.to_string())
        .execute(&mut *tx)
        .await
        .map_err(db_error)?
        .rows_affected();
        if changed != 1 {
            return Err(Error::InvalidFormat);
        }
        sqlx::query("DELETE FROM write_intents WHERE operation_id=?")
            .bind(intent.operation_id.to_string())
            .execute(&mut *tx)
            .await
            .map_err(db_error)?;
        tx.commit().await.map_err(db_error)?;
        Ok(Receipt {
            operation_id: intent.operation_id,
            outcome,
        })
    }
}

fn document_post_state(document: &Document) -> Result<String, Error> {
    let mut value = serde_json::to_value(document).map_err(db_error)?;
    value["kind"] = "file".into();
    value["deleted"] = false.into();
    value["children_generation"] = "0".into();
    value["size"] = document.size.to_string().into();
    encode(&value)
}

async fn validate_layout(connection: &mut SqliteConnection, root_id: NodeId) -> Result<(), Error> {
    let head = validate_journal(connection).await?;
    validate_nodes(connection, root_id, head).await?;
    validate_streams(connection).await
}

async fn validate_journal(connection: &mut SqliteConnection) -> Result<u64, Error> {
    let head: String = sqlx::query_scalar("SELECT head_seq FROM workspaces WHERE singleton=1")
        .fetch_one(&mut *connection)
        .await
        .map_err(db_error)?;
    let head = canonical_u64(&head)?;
    let mut sequences: Vec<u64> = sqlx::query_scalar::<_, String>("SELECT seq FROM change_sets")
        .fetch_all(&mut *connection)
        .await
        .map_err(db_error)?
        .iter()
        .map(|s| canonical_u64(s))
        .collect::<Result<_, _>>()?;
    sequences.sort_unstable();
    if sequences.len() as u64 != head
        || sequences
            .iter()
            .enumerate()
            .any(|(index, seq)| *seq != index as u64 + 1)
    {
        return Err(Error::InvalidFormat);
    }
    Ok(head)
}

async fn validate_nodes(
    connection: &mut SqliteConnection,
    root_id: NodeId,
    head: u64,
) -> Result<(), Error> {
    let nodes = sqlx::query("SELECT * FROM nodes")
        .fetch_all(&mut *connection)
        .await
        .map_err(db_error)?;
    let mut roots = 0;
    let mut children = 0;
    let mut expected_children = None;
    for node in &nodes {
        let id: String = node.try_get("node_id").map_err(db_error)?;
        let _: NodeId = parse(&id)?;
        let parent: Option<String> = node.try_get("parent_id").map_err(db_error)?;
        let kind: String = node.try_get("kind").map_err(db_error)?;
        let name: String = node.try_get("name").map_err(db_error)?;
        let key: String = node.try_get("normalized_name_key").map_err(db_error)?;
        let meta: String = node.try_get("meta_rev").map_err(db_error)?;
        let seq = canonical_u64(node.try_get("last_change_seq").map_err(db_error)?)?;
        let generation = canonical_u64(node.try_get("children_generation").map_err(db_error)?)?;
        if seq > head || meta.is_empty() {
            return Err(Error::InvalidFormat);
        }
        match parent {
            None => {
                roots += 1;
                if id != root_id.to_string()
                    || kind != "folder"
                    || !name.is_empty()
                    || !key.is_empty()
                    || node
                        .try_get::<Option<String>, _>("content_rev")
                        .map_err(db_error)?
                        .is_some()
                    || node
                        .try_get::<Option<String>, _>("content_hash")
                        .map_err(db_error)?
                        .is_some()
                    || node
                        .try_get::<Option<String>, _>("size")
                        .map_err(db_error)?
                        .is_some()
                {
                    return Err(Error::InvalidFormat);
                }
                expected_children = Some(generation);
            }
            Some(parent) => {
                children += 1;
                if parent != root_id.to_string()
                    || kind != "file"
                    || generation != 0
                    || seq == 0
                    || notryx_domain::mutation::validate_name(&name).is_err()
                    || key != notryx_domain::mutation::normalized_name_key(&name)
                {
                    return Err(Error::InvalidFormat);
                }
                canonical_u64(node.try_get("size").map_err(db_error)?)?;
                let revision: String = node.try_get("content_rev").map_err(db_error)?;
                if revision.is_empty() {
                    return Err(Error::InvalidFormat);
                }
            }
        }
        if seq != 0 {
            let post: Option<String> =
                sqlx::query_scalar("SELECT post_state FROM node_changes WHERE seq=? AND node_id=?")
                    .bind(seq.to_string())
                    .bind(&id)
                    .fetch_optional(&mut *connection)
                    .await
                    .map_err(db_error)?;
            let value: serde_json::Value = decode(&post.ok_or(Error::InvalidFormat)?)?;
            let nullable = |column: &str| -> Result<serde_json::Value, Error> {
                Ok(node
                    .try_get::<Option<String>, _>(column)
                    .map_err(db_error)?
                    .map_or(serde_json::Value::Null, Into::into))
            };
            let expected = serde_json::json!({"node_id":id,"parent_id":nullable("parent_id")?,"name":name,
                "kind":kind,"meta_rev":meta,"content_rev":nullable("content_rev")?,"content_hash":nullable("content_hash")?,
                "size":nullable("size")?,"deleted":false,"children_generation":generation.to_string(),"last_change_seq":seq.to_string()});
            if value != expected {
                return Err(Error::InvalidFormat);
            }
        }
    }
    if roots != 1 || expected_children != Some(children) {
        return Err(Error::InvalidFormat);
    }
    Ok(())
}

async fn validate_streams(connection: &mut SqliteConnection) -> Result<(), Error> {
    let streams = sqlx::query("SELECT stream_id,last_seq FROM operation_streams")
        .fetch_all(&mut *connection)
        .await
        .map_err(db_error)?;
    for stream in streams {
        let id: String = stream.try_get("stream_id").map_err(db_error)?;
        let parsed: Uuid = parse(&id)?;
        if parsed.get_version_num() != 4 || parsed.get_variant() != uuid::Variant::RFC4122 {
            return Err(Error::InvalidFormat);
        }
        let last = canonical_u64(stream.try_get("last_seq").map_err(db_error)?)?;
        let rows = sqlx::query(
            "SELECT operation_seq,outcome,pending FROM operation_receipts WHERE stream_id=?",
        )
        .bind(id)
        .fetch_all(&mut *connection)
        .await
        .map_err(db_error)?;
        let mut sequences = Vec::new();
        for row in rows {
            sequences.push(canonical_u64(
                row.try_get("operation_seq").map_err(db_error)?,
            )?);
            let outcome: Outcome = decode(row.try_get("outcome").map_err(db_error)?)?;
            let pending: i64 = row.try_get("pending").map_err(db_error)?;
            if (pending == 1) != (outcome == Outcome::Pending) {
                return Err(Error::InvalidFormat);
            }
        }
        sequences.sort_unstable();
        if sequences.len() as u64 != last
            || sequences
                .iter()
                .enumerate()
                .any(|(index, seq)| *seq != index as u64 + 1)
        {
            return Err(Error::InvalidFormat);
        }
    }
    Ok(())
}

fn canonical_u64(value: &str) -> Result<u64, Error> {
    let number: u64 = parse(value)?;
    if number.to_string() != value {
        return Err(Error::InvalidFormat);
    }
    Ok(number)
}
