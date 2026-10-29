use super::{Document, Error, Outcome, Receipt, SaveContent};
use notryx_domain::{ChangeSeq, NodeId, WorkspaceEpoch, WorkspaceId};
use serde::{Deserialize, Serialize};
use sqlx::{
    Connection, Row, SqliteConnection,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous},
};
use std::{fs::OpenOptions, os::unix::fs::OpenOptionsExt, path::Path, time::Duration};
use uuid::Uuid;

pub(crate) mod auth;
pub(crate) mod create;
pub(crate) mod lifecycle;
pub(crate) mod move_node;
pub(crate) mod node;
pub(crate) mod recovery;
#[cfg(test)]
mod sqlite_corruption_tests;
mod stream;
pub(crate) mod upload;
use create::CreateIntent;

const APPLICATION_ID: i64 = 0x4e545253;
pub(crate) const CURRENT_SCHEMA_VERSION: i64 = 10;

pub(crate) fn db_error(error: sqlx::Error) -> Error {
    // SQLite extended result codes retain the primary result in their low byte.
    let primary = error
        .as_database_error()
        .and_then(|error| error.code())
        .and_then(|code| code.parse::<i32>().ok())
        .map(|code| code & 0xff);
    match primary {
        Some(11 | 26) => Error::InvalidFormat,
        Some(13) => Error::NoSpace,
        _ => Error::Storage,
    }
}
pub(crate) fn serialization_error(_: serde_json::Error) -> Error {
    Error::Storage
}
fn io_error(error: std::io::Error) -> Error {
    if error.raw_os_error().is_some_and(|code| {
        code == rustix::io::Errno::NOSPC.raw_os_error()
            || code == rustix::io::Errno::DQUOT.raw_os_error()
    }) {
        Error::NoSpace
    } else {
        Error::Storage
    }
}
fn decode<T: serde::de::DeserializeOwned>(text: &str) -> Result<T, Error> {
    serde_json::from_str(text).map_err(|_| Error::InvalidFormat)
}
fn encode(value: &impl Serialize) -> Result<String, Error> {
    serde_json::to_string(value).map_err(serialization_error)
}
fn parse<T: std::str::FromStr>(value: &str) -> Result<T, Error> {
    value.parse().map_err(|_| Error::InvalidFormat)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Intent {
    pub operation_id: notryx_domain::OperationId,
    pub before: Document,
    pub proposed: Document,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub(crate) enum PendingIntent {
    Save(Intent),
    Create(CreateIntent),
    Move(move_node::MoveIntent),
    Lifecycle(lifecycle::LifecycleIntent),
}

impl PendingIntent {
    pub(crate) fn operation_id(&self) -> notryx_domain::OperationId {
        match self {
            Self::Save(intent) => intent.operation_id,
            Self::Create(intent) => intent.operation_id,
            Self::Move(intent) => intent.operation_id,
            Self::Lifecycle(intent) => intent.operation_id,
        }
    }
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
            .map_err(io_error)?;
        file.sync_all().map_err(io_error)?;
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
                deleted INTEGER NOT NULL DEFAULT 0 CHECK(deleted IN(0,1)));
            CREATE UNIQUE INDEX live_node_name ON nodes(parent_id, normalized_name_key) WHERE deleted=0;
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
        sqlx::query("INSERT INTO nodes VALUES(?,NULL,'','','folder',NULL,?,NULL,NULL,'0','0',0)")
            .bind(format!("{root_id}"))
            .bind(Uuid::new_v4().to_string())
            .execute(&mut *tx)
            .await
            .map_err(db_error)?;
        auth::initialize(&mut tx, workspace_id).await?;
        stream::initialize(&mut tx).await?;
        stream::migrate_receipt_etags(&mut tx).await?;
        recovery::initialize(&mut tx).await?;
        upload::initialize(&mut tx).await?;
        initialize_retention(&mut tx, root_id).await?;
        sqlx::query("PRAGMA user_version=10")
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
        Self::open_version(path, expected_id, false)
            .await
            .map(|(state, _)| state)
    }

    pub async fn open_for_migration(
        path: &Path,
        expected_id: WorkspaceId,
    ) -> Result<(Self, i64), Error> {
        Self::open_version(path, expected_id, true).await
    }

    async fn open_version(
        path: &Path,
        expected_id: WorkspaceId,
        allow_legacy: bool,
    ) -> Result<(Self, i64), Error> {
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
        if app_id != APPLICATION_ID
            || !(1..=CURRENT_SCHEMA_VERSION).contains(&version)
            || check != "ok"
        {
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
        validate_layout(&mut probe, root_id, version).await?;
        if version >= 2 {
            auth::validate(&mut probe, workspace_id).await?;
        }
        if version < CURRENT_SCHEMA_VERSION && !allow_legacy {
            return Err(Error::MigrationRequired);
        }
        probe.close().await.map_err(db_error)?;
        Ok((
            Self {
                connection: Self::connect(path).await?,
                workspace_id,
                epoch,
                root_id,
            },
            version,
        ))
    }

    pub async fn migrate(&mut self, version: i64) -> Result<(), Error> {
        // SQLite requires foreign keys to be disabled before the rebuild transaction.
        sqlx::query("PRAGMA foreign_keys=OFF")
            .execute(&mut self.connection)
            .await
            .map_err(db_error)?;
        let result = self.migrate_transaction(version).await;
        let restored = sqlx::query("PRAGMA foreign_keys=ON")
            .execute(&mut self.connection)
            .await
            .map_err(db_error);
        restored?;
        result
    }

    async fn migrate_transaction(&mut self, version: i64) -> Result<(), Error> {
        let mut tx = self.connection.begin().await.map_err(db_error)?;
        if version == 1 {
            auth::initialize(&mut tx, self.workspace_id).await?;
        }
        if version < 3 {
            stream::initialize(&mut tx).await?;
        }
        if version < 5 {
            stream::migrate_receipt_etags(&mut tx).await?;
        }
        if version < 7 {
            sqlx::raw_sql("CREATE TABLE nodes_new (
                node_id TEXT PRIMARY KEY NOT NULL, parent_id TEXT REFERENCES nodes(node_id),
                name TEXT NOT NULL, normalized_name_key TEXT NOT NULL, kind TEXT NOT NULL,
                content_rev TEXT, meta_rev TEXT NOT NULL, content_hash TEXT, size TEXT,
                children_generation TEXT NOT NULL, last_change_seq TEXT NOT NULL,
                deleted INTEGER NOT NULL DEFAULT 0 CHECK(deleted IN(0,1)));
                INSERT INTO nodes_new SELECT node_id,parent_id,name,normalized_name_key,kind,
                    content_rev,meta_rev,content_hash,size,children_generation,last_change_seq,0 FROM nodes;
                DROP TABLE nodes;
                ALTER TABLE nodes_new RENAME TO nodes;
                CREATE UNIQUE INDEX live_node_name ON nodes(parent_id, normalized_name_key) WHERE deleted=0;")
                .execute(&mut *tx).await.map_err(db_error)?;
        }
        if version < 8 {
            recovery::initialize(&mut tx).await?;
            recovery::migrate_existing(&mut tx).await?;
        }
        if version < 9 {
            upload::initialize(&mut tx).await?;
        }
        if version < 10 {
            initialize_retention(&mut tx, self.root_id).await?;
        }
        if !sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(&mut *tx)
            .await
            .map_err(db_error)?
            .is_empty()
        {
            return Err(Error::InvalidFormat);
        }
        auth::validate(&mut tx, self.workspace_id).await?;
        validate_layout(&mut tx, self.root_id, CURRENT_SCHEMA_VERSION).await?;
        sqlx::query("PRAGMA user_version=10")
            .execute(&mut *tx)
            .await
            .map_err(db_error)?;
        tx.commit().await.map_err(db_error)
    }

    pub async fn head_seq(&mut self) -> Result<ChangeSeq, Error> {
        let head: String = sqlx::query_scalar("SELECT head_seq FROM workspaces WHERE singleton=1")
            .fetch_one(&mut self.connection)
            .await
            .map_err(db_error)?;
        Ok(ChangeSeq::new(parse(&head)?))
    }

    pub async fn next_change_seq(&mut self) -> Result<ChangeSeq, Error> {
        let head = self.head_seq().await?.get();
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
        let row = match row {
            Some(row) if node::snapshot(&row)?.deleted => None,
            other => other,
        };
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

    pub async fn retained_object_is_referenced(&mut self, hash: &str) -> Result<bool, Error> {
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM nodes WHERE content_hash=? UNION ALL SELECT 1 FROM recovery_objects WHERE hash=?)")
            .bind(hash)
            .bind(hash)
            .fetch_one(&mut self.connection)
            .await
            .map_err(db_error)
    }

    pub async fn documents(&mut self) -> Result<Vec<Document>, Error> {
        let ids: Vec<String> = sqlx::query_scalar("SELECT node_id FROM nodes WHERE kind='file'")
            .fetch_all(&mut self.connection)
            .await
            .map_err(db_error)?;
        let mut docs = Vec::new();
        for id in ids {
            if let Some(document) = self.document(parse(&id)?).await? {
                docs.push(document);
            }
        }
        Ok(docs)
    }

    pub async fn adopt(&mut self, document: &Document) -> Result<(), Error> {
        let mut tx = self.connection.begin().await.map_err(db_error)?;
        let seq = document.last_change_seq.get().to_string();
        sqlx::query("INSERT INTO nodes VALUES(?,?,?,?,'file',?,?,?,?,'0',?,0)")
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
        sqlx::query("INSERT INTO change_sets(seq,operation_id,committed_at) VALUES(?,NULL,NULL)")
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
        let root = node::node_post_state(&crate::NodeSnapshot {
            node_id: self.root_id,
            parent_id: None,
            name: String::new(),
            kind: notryx_protocol::NodeKind::Folder,
            content_rev: None,
            meta_rev: row
                .try_get::<String, _>("meta_rev")
                .map_err(db_error)?
                .try_into()
                .map_err(|_| Error::InvalidFormat)?,
            content_hash: None,
            size: None,
            children_generation: generation,
            last_change_seq: document.last_change_seq,
            deleted: false,
        });
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

    pub async fn finish_external_file(
        &mut self,
        before: &Document,
        proposed: &Document,
        #[cfg(test)] hook: &mut Option<crate::FailureHook>,
    ) -> Result<(), Error> {
        let mut tx = self.connection.begin().await.map_err(db_error)?;
        let seq = proposed.last_change_seq.get();
        let changed = sqlx::query("UPDATE nodes SET content_rev=?,content_hash=?,size=?,last_change_seq=? WHERE node_id=? AND kind='file' AND deleted=0 AND parent_id=? AND name=? AND content_rev=? AND meta_rev=? AND content_hash=? AND size=? AND last_change_seq=?")
            .bind(proposed.content_rev.as_str()).bind(&proposed.content_hash).bind(proposed.size.to_string()).bind(seq.to_string())
            .bind(before.node_id.to_string()).bind(before.parent_id.to_string()).bind(&before.name)
            .bind(before.content_rev.as_str()).bind(before.meta_rev.as_str()).bind(&before.content_hash)
            .bind(before.size.to_string()).bind(before.last_change_seq.get().to_string())
            .execute(&mut *tx).await.map_err(db_error)?.rows_affected();
        if changed != 1 {
            return Err(Error::ExternalChange);
        }
        #[cfg(test)]
        if let Some(hook) = hook {
            hook(crate::Phase::BeforeRecoveryFinish)?;
        }
        sqlx::query("INSERT INTO change_sets(seq,operation_id,committed_at) VALUES(?,NULL,NULL)")
            .bind(seq.to_string())
            .execute(&mut *tx)
            .await
            .map_err(db_error)?;
        sqlx::query("INSERT INTO node_changes VALUES(?,?,?)")
            .bind(seq.to_string())
            .bind(proposed.node_id.to_string())
            .bind(document_post_state(proposed)?)
            .execute(&mut *tx)
            .await
            .map_err(db_error)?;
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

    pub async fn admit(
        &mut self,
        request: &SaveContent,
        fingerprint: &str,
        outcome: &Outcome,
        intent: Option<&Intent>,
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

    pub async fn pending(&mut self) -> Result<Vec<PendingIntent>, Error> {
        let rows = sqlx::query("SELECT operation_id,intent FROM write_intents")
            .fetch_all(&mut self.connection)
            .await
            .map_err(db_error)?;
        rows.into_iter()
            .map(|row| {
                let intent: PendingIntent = decode(row.try_get("intent").map_err(db_error)?)?;
                if row.try_get::<&str, _>("operation_id").map_err(db_error)?
                    != intent.operation_id().to_string()
                {
                    return Err(Error::InvalidFormat);
                }
                Ok(intent)
            })
            .collect()
    }

    pub async fn quarantine(
        &mut self,
        operation_id: notryx_domain::OperationId,
        observed_hash: Option<&str>,
    ) -> Result<(), Error> {
        self.quarantine_intent(operation_id, observed_hash, None)
            .await
    }

    pub async fn is_quarantined(
        &mut self,
        operation_id: notryx_domain::OperationId,
    ) -> Result<bool, Error> {
        let flag: i64 =
            sqlx::query_scalar("SELECT recovery_conflict FROM write_intents WHERE operation_id=?")
                .bind(operation_id.to_string())
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
            sqlx::query("INSERT INTO change_sets(seq,operation_id,committed_at) VALUES(?,?,NULL)")
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
        stream::finish_pending(&mut tx, intent.operation_id, &outcome).await?;
        tx.commit().await.map_err(db_error)?;
        Ok(Receipt {
            operation_id: intent.operation_id,
            outcome,
        })
    }
}

fn document_post_state(document: &Document) -> Result<String, Error> {
    encode(&node::node_post_state(&crate::NodeSnapshot::from(document)))
}

async fn validate_layout(
    connection: &mut SqliteConnection,
    root_id: NodeId,
    version: i64,
) -> Result<(), Error> {
    if version >= 7 {
        let index: Option<String> = sqlx::query_scalar(
            "SELECT sql FROM sqlite_master WHERE type='index' AND name='live_node_name'",
        )
        .fetch_optional(&mut *connection)
        .await
        .map_err(db_error)?;
        if index.as_deref()
            != Some(
                "CREATE UNIQUE INDEX live_node_name ON nodes(parent_id, normalized_name_key) WHERE deleted=0",
            )
        {
            return Err(Error::InvalidFormat);
        }
        let deleted: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pragma_table_info('nodes') WHERE name='deleted' AND [notnull]=1",
        )
        .fetch_one(&mut *connection)
        .await
        .map_err(db_error)?;
        if deleted != 1 {
            return Err(Error::InvalidFormat);
        }
    }
    let head = validate_journal(connection, version).await?;
    node::validate_tree(connection, root_id, head, version).await?;
    if version >= 10 {
        validate_retention(connection, root_id).await?;
    }
    stream::validate(connection, version).await?;
    if version >= 8 {
        recovery::validate(connection).await?;
    }
    if version >= 9 {
        upload::validate(connection).await?;
    }
    Ok(())
}

async fn retention_initial_root(
    connection: &mut SqliteConnection,
    root_id: NodeId,
) -> Result<serde_json::Value, Error> {
    let row = sqlx::query("SELECT node_id,parent_id,name,kind,content_rev,meta_rev,content_hash,size,children_generation,last_change_seq,deleted FROM nodes WHERE node_id=?")
        .bind(root_id.to_string())
        .fetch_one(connection)
        .await
        .map_err(db_error)?;
    let mut root = node::snapshot(&row)?;
    if root.node_id != root_id
        || root.parent_id.is_some()
        || !root.name.is_empty()
        || root.kind != notryx_protocol::NodeKind::Folder
        || root.deleted
    {
        return Err(Error::InvalidFormat);
    }
    root.children_generation = 0;
    root.last_change_seq = ChangeSeq::new(0);
    Ok(node::node_post_state(&root))
}

async fn initialize_retention(
    connection: &mut SqliteConnection,
    root_id: NodeId,
) -> Result<(), Error> {
    sqlx::raw_sql("ALTER TABLE workspaces ADD COLUMN journal_floor_seq TEXT NOT NULL DEFAULT '0';
        ALTER TABLE workspaces ADD COLUMN journal_baseline_count INTEGER NOT NULL DEFAULT 0 CHECK(typeof(journal_baseline_count)='integer' AND journal_baseline_count>=0);
        ALTER TABLE workspaces ADD COLUMN journal_baseline_hash TEXT NOT NULL DEFAULT '';
        ALTER TABLE change_sets ADD COLUMN committed_at INTEGER CHECK(committed_at IS NULL OR (typeof(committed_at)='integer' AND committed_at>=0));
        CREATE TABLE journal_baseline (node_id TEXT PRIMARY KEY NOT NULL REFERENCES nodes(node_id), seq TEXT NOT NULL, post_state TEXT NOT NULL);")
        .execute(&mut *connection).await.map_err(db_error)?;
    let root = retention_initial_root(connection, root_id).await?;
    sqlx::query("INSERT INTO journal_baseline(node_id,seq,post_state) VALUES (?,'0',?)")
        .bind(root_id.to_string())
        .bind(encode(&root)?)
        .execute(&mut *connection)
        .await
        .map_err(db_error)?;
    let (count, digest) = crate::retention::baseline_digest(connection, 0).await?;
    sqlx::query(
        "UPDATE workspaces SET journal_baseline_count=?,journal_baseline_hash=? WHERE singleton=1",
    )
    .bind(count)
    .bind(digest)
    .execute(connection)
    .await
    .map_err(db_error)?;
    Ok(())
}

pub(crate) async fn validate_retention(
    connection: &mut SqliteConnection,
    root_id: NodeId,
) -> Result<(), Error> {
    let floor: String =
        sqlx::query_scalar("SELECT journal_floor_seq FROM workspaces WHERE singleton=1")
            .fetch_one(&mut *connection)
            .await
            .map_err(db_error)?;
    let floor = canonical_u64(&floor)?;
    let head: String = sqlx::query_scalar("SELECT head_seq FROM workspaces WHERE singleton=1")
        .fetch_one(&mut *connection)
        .await
        .map_err(db_error)?;
    if floor > canonical_u64(&head)? {
        return Err(Error::InvalidFormat);
    }
    let expected = sqlx::query(
        "SELECT journal_baseline_count,journal_baseline_hash FROM workspaces WHERE singleton=1",
    )
    .fetch_one(&mut *connection)
    .await
    .map_err(db_error)?;
    let expected_count: i64 = expected
        .try_get("journal_baseline_count")
        .map_err(db_error)?;
    let expected_hash: String = expected
        .try_get("journal_baseline_hash")
        .map_err(db_error)?;
    let (actual_count, actual_hash) = crate::retention::baseline_digest(connection, floor).await?;
    if expected_count < 1 || expected_count != actual_count || expected_hash != actual_hash {
        return Err(Error::InvalidFormat);
    }
    let invalid: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM change_sets WHERE committed_at IS NOT NULL AND (typeof(committed_at)<>'integer' OR committed_at<0)")
        .fetch_one(&mut *connection).await.map_err(db_error)?;
    if invalid != 0 {
        return Err(Error::InvalidFormat);
    }
    let rows = sqlx::query("SELECT node_id,seq,post_state FROM journal_baseline")
        .fetch_all(&mut *connection)
        .await
        .map_err(db_error)?;
    if floor == 0 {
        if rows.len() != 1
            || rows[0].try_get::<String, _>("node_id").map_err(db_error)? != root_id.to_string()
            || rows[0].try_get::<String, _>("seq").map_err(db_error)? != "0"
        {
            return Err(Error::InvalidFormat);
        }
        let actual: String = rows[0].try_get("post_state").map_err(db_error)?;
        if decode::<serde_json::Value>(&actual)?
            != retention_initial_root(connection, root_id).await?
        {
            return Err(Error::InvalidFormat);
        }
    } else if rows.is_empty() {
        return Err(Error::InvalidFormat);
    }
    Ok(())
}

pub(crate) async fn validate_journal(
    connection: &mut SqliteConnection,
    version: i64,
) -> Result<u64, Error> {
    let head: String = sqlx::query_scalar("SELECT head_seq FROM workspaces WHERE singleton=1")
        .fetch_one(&mut *connection)
        .await
        .map_err(db_error)?;
    let head = canonical_u64(&head)?;
    let has_floor: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pragma_table_info('workspaces') WHERE name='journal_floor_seq'",
    )
    .fetch_one(&mut *connection)
    .await
    .map_err(db_error)?;
    let floor = if version >= 10 && has_floor == 1 {
        let value: String =
            sqlx::query_scalar("SELECT journal_floor_seq FROM workspaces WHERE singleton=1")
                .fetch_one(&mut *connection)
                .await
                .map_err(db_error)?;
        canonical_u64(&value)?
    } else if version < 10 {
        0
    } else {
        return Err(Error::InvalidFormat);
    };
    let retained = head.checked_sub(floor).ok_or(Error::InvalidFormat)?;
    let mut sequences: Vec<u64> = sqlx::query_scalar::<_, String>("SELECT seq FROM change_sets")
        .fetch_all(&mut *connection)
        .await
        .map_err(db_error)?
        .iter()
        .map(|s| canonical_u64(s))
        .collect::<Result<_, _>>()?;
    sequences.sort_unstable();
    if sequences.len() as u64 != retained
        || sequences
            .iter()
            .enumerate()
            .any(|(index, seq)| floor.checked_add(index as u64 + 1) != Some(*seq))
    {
        return Err(Error::InvalidFormat);
    }
    Ok(head)
}

fn canonical_u64(value: &str) -> Result<u64, Error> {
    let number: u64 = parse(value)?;
    if number.to_string() != value {
        return Err(Error::InvalidFormat);
    }
    Ok(number)
}
