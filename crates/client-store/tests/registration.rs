use notryx_client_store::{ClientStore, RegistrationBinding, StoreError};
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use uuid::Uuid;

fn binding() -> RegistrationBinding {
    RegistrationBinding {
        server_origin: "https://workspace.example".into(),
        workspace_id: Uuid::new_v4().to_string(),
        workspace_epoch: Uuid::new_v4().to_string(),
        device_id: Uuid::new_v4().to_string(),
        device_stream_id: Uuid::new_v4().to_string(),
    }
}

#[tokio::test]
async fn reopening_a_registered_draft_preserves_its_base_and_rejects_duplicate_sessions() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("drafts.db");
    let registration = binding();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&registration).await.unwrap();
    let opened = store
        .open_registered_document(&registration, "doc", b"original base", "original-rev")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(&registration, "doc", &opened.edit_session_id, 7, b"draft")
        .await
        .unwrap();
    store
        .close_registered_document(&registration, "doc", &opened.edit_session_id)
        .await
        .unwrap();
    store.close().await.unwrap();

    let mut store = ClientStore::open(&path).await.unwrap();
    let (reopened, active) = store
        .try_open_registered_draft(&registration, "doc")
        .await
        .unwrap()
        .unwrap();
    assert!(active);
    assert_eq!(reopened.bytes, b"draft");
    assert_eq!(reopened.generation, 7);
    assert_eq!(
        store.try_open_registered_draft(&registration, "doc").await,
        Err(StoreError::SessionAlreadyOpen)
    );
    store
        .save_registered_checkpoint(&registration, "doc", &reopened.edit_session_id, 8, b"next")
        .await
        .unwrap();
    store
        .close_registered_document(&registration, "doc", &reopened.edit_session_id)
        .await
        .unwrap();
    let saved = store
        .read_registered_checkpoint(&registration, "doc")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(saved.bytes, b"next");
    assert_eq!(saved.generation, 8);
    assert_eq!(saved.base_bytes, b"original base");
    assert_eq!(saved.base_revision, "original-rev");
    let queued = store
        .queue_checkpoint(&registration, "doc", 8)
        .await
        .unwrap();
    assert_eq!(queued.bytes, b"next");
    assert_eq!(queued.expected_revision.as_deref(), Some("original-rev"));
    assert!(
        store
            .try_open_registered_draft(&registration, "missing")
            .await
            .unwrap()
            .is_none()
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn missing_persisted_base_is_not_replaced_by_server_content() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("drafts.db");
    let registration = binding();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&registration).await.unwrap();
    let opened = store
        .open_registered_document(&registration, "doc", b"original base", "original-rev")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(&registration, "doc", &opened.edit_session_id, 1, b"draft")
        .await
        .unwrap();
    store
        .close_registered_document(&registration, "doc", &opened.edit_session_id)
        .await
        .unwrap();
    store.close().await.unwrap();

    let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
        .await
        .unwrap();
    sqlx::query("DELETE FROM draft_bases")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    assert!(
        store
            .try_open_registered_draft(&registration, "doc")
            .await
            .is_err()
    );
    store.close().await.unwrap();
    let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
        .await
        .unwrap();
    let bytes: Vec<u8> = sqlx::query_scalar("SELECT bytes FROM draft_checkpoints")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(bytes, b"draft");
    db.close().await.unwrap();
}

#[tokio::test]
async fn every_binding_field_is_part_of_draft_identity() {
    for field in 0..5 {
        let directory = tempfile::tempdir().unwrap();
        let mut store = ClientStore::open(directory.path().join("drafts.db"))
            .await
            .unwrap();
        let first = binding();
        let mut next = first.clone();
        match field {
            0 => next.server_origin = "https://other.example".into(),
            1 => next.workspace_id = Uuid::new_v4().to_string(),
            2 => next.workspace_epoch = Uuid::new_v4().to_string(),
            3 => next.device_id = Uuid::new_v4().to_string(),
            4 => next.device_stream_id = Uuid::new_v4().to_string(),
            _ => unreachable!(),
        }
        store.activate_registration(&first).await.unwrap();
        let opened = store
            .open_registered_document(&first, "doc", b"first base", "rev-a")
            .await
            .unwrap();
        store
            .save_registered_checkpoint(&first, "doc", &opened.edit_session_id, 1, b"first draft")
            .await
            .unwrap();
        store
            .close_registered_document(&first, "doc", &opened.edit_session_id)
            .await
            .unwrap();
        store.activate_registration(&next).await.unwrap();
        let opened = store
            .open_registered_document(&next, "doc", b"second base", "rev-b")
            .await
            .unwrap();
        assert_eq!(opened.bytes, b"second base", "binding field {field}");
        assert_eq!(opened.generation, 0, "binding field {field}");
        assert_eq!(
            store
                .read_registered_checkpoint(&first, "doc")
                .await
                .unwrap()
                .unwrap()
                .bytes,
            b"first draft"
        );
        store.close().await.unwrap();
    }
}

#[tokio::test]
async fn different_workspaces_must_not_share_a_document_draft() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("drafts.db");
    let first = binding();
    let second = binding();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&first).await.unwrap();
    let opened = store
        .open_registered_document(&first, "same-node", b"workspace a base", "rev-a")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(
            &first,
            "same-node",
            &opened.edit_session_id,
            1,
            b"workspace a draft",
        )
        .await
        .unwrap();
    store
        .close_registered_document(&first, "same-node", &opened.edit_session_id)
        .await
        .unwrap();
    store.activate_registration(&second).await.unwrap();
    let other = store
        .open_registered_document(&second, "same-node", b"workspace b base", "rev-b")
        .await
        .unwrap();
    assert_eq!(other.bytes, b"workspace b base");
    assert_eq!(other.generation, 0);
    assert_eq!(
        store
            .save_registered_checkpoint(
                &first,
                "same-node",
                &opened.edit_session_id,
                2,
                b"late response"
            )
            .await,
        Err(StoreError::StaleRegistration)
    );
    store
        .save_registered_checkpoint(
            &second,
            "same-node",
            &other.edit_session_id,
            1,
            b"workspace b draft",
        )
        .await
        .unwrap();
    store
        .close_registered_document(&second, "same-node", &other.edit_session_id)
        .await
        .unwrap();
    assert_eq!(
        store.activate_registration(&first).await,
        Err(StoreError::StaleRegistration)
    );
    store.close().await.unwrap();

    let mut reopened = ClientStore::open(&path).await.unwrap();
    assert_eq!(
        reopened.current_registration().await.unwrap(),
        Some(second.clone())
    );
    let old = reopened
        .read_registered_checkpoint(&first, "same-node")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(old.bytes, b"workspace a draft");
    assert_eq!(old.base_bytes, b"workspace a base");
    assert_eq!(old.base_revision, "rev-a");
    let current = reopened
        .read_registered_checkpoint(&second, "same-node")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.bytes, b"workspace b draft");
    assert_eq!(current.base_bytes, b"workspace b base");
    let listed = reopened.list_registered_drafts().await.unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].binding, first);
    assert_eq!(listed[1].binding, second);
    assert!(
        listed
            .iter()
            .all(|draft| draft.document_id == "same-node" && draft.generation == 1)
    );
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn reregistration_and_restore_preserve_old_drafts_without_auto_linking() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = ClientStore::open(directory.path().join("drafts.db"))
        .await
        .unwrap();
    let original = binding();
    let mut re_pair = original.clone();
    re_pair.device_id = Uuid::new_v4().to_string();
    re_pair.device_stream_id = Uuid::new_v4().to_string();
    let mut restore = re_pair.clone();
    restore.workspace_epoch = Uuid::new_v4().to_string();
    restore.device_id = Uuid::new_v4().to_string();
    restore.device_stream_id = Uuid::new_v4().to_string();
    store.activate_registration(&original).await.unwrap();
    let opened = store
        .open_registered_document(&original, "doc", b"base", "base-rev")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(&original, "doc", &opened.edit_session_id, 4, b"dirty")
        .await
        .unwrap();
    assert_eq!(
        store.activate_registration(&re_pair).await,
        Err(StoreError::OpenEditSessions)
    );
    assert_eq!(
        store.current_registration().await.unwrap(),
        Some(original.clone())
    );
    store
        .close_registered_document(&original, "doc", &opened.edit_session_id)
        .await
        .unwrap();
    store.activate_registration(&re_pair).await.unwrap();
    let new = store
        .open_registered_document(&re_pair, "doc", b"current remote", "new-rev")
        .await
        .unwrap();
    assert_eq!(new.bytes, b"current remote");
    assert_eq!(new.generation, 0);
    assert_eq!(
        store
            .close_registered_document(&original, "doc", &opened.edit_session_id)
            .await,
        Err(StoreError::StaleRegistration)
    );
    store
        .close_registered_document(&re_pair, "doc", &new.edit_session_id)
        .await
        .unwrap();
    store.activate_registration(&restore).await.unwrap();
    let after_restore = store
        .open_registered_document(&restore, "doc", b"restored remote", "restored-rev")
        .await
        .unwrap();
    assert_eq!(after_restore.bytes, b"restored remote");
    assert_eq!(after_restore.generation, 0);
    let saved = store
        .read_registered_checkpoint(&original, "doc")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(saved.generation, 4);
    assert_eq!(saved.bytes, b"dirty");
    assert_eq!(saved.base_bytes, b"base");
    assert_eq!(saved.base_revision, "base-rev");
    store.close().await.unwrap();
}

#[tokio::test]
async fn reload_reopens_only_the_active_registrations_durable_checkpoint() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("drafts.db");
    let registration = binding();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&registration).await.unwrap();
    let opened = store
        .open_registered_document(&registration, "doc", b"base", "rev")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(
            &registration,
            "doc",
            &opened.edit_session_id,
            5,
            b"committed",
        )
        .await
        .unwrap();
    store.reset_edit_sessions();
    let reopened = store
        .open_registered_document(&registration, "doc", b"changed remote", "new-rev")
        .await
        .unwrap();
    assert_eq!(reopened.generation, 5);
    assert_eq!(reopened.bytes, b"committed");
    assert_eq!(
        store
            .save_registered_checkpoint(
                &registration,
                "doc",
                &opened.edit_session_id,
                6,
                b"old renderer"
            )
            .await,
        Err(StoreError::StaleSession)
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn rejects_credential_bearing_or_noncanonical_registration_without_writing() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = ClientStore::open(directory.path().join("drafts.db"))
        .await
        .unwrap();
    let mut bad = binding();
    bad.server_origin = "https://name:secret@workspace.example".into();
    assert_eq!(
        store.activate_registration(&bad).await,
        Err(StoreError::InvalidRegistration)
    );
    bad.server_origin = "https://workspace.example/path".into();
    assert_eq!(
        store.activate_registration(&bad).await,
        Err(StoreError::InvalidRegistration)
    );
    bad.server_origin = "https://workspace.example".into();
    bad.workspace_id = "not-a-uuid".into();
    assert_eq!(
        store.activate_registration(&bad).await,
        Err(StoreError::InvalidRegistration)
    );
    assert_eq!(store.current_registration().await.unwrap(), None);
    for origin in [
        "HTTPS://workspace.example",
        "https://WORKSPACE.example",
        "https://workspace.example:443",
        "https://workspace.example/",
        "https://[malformed",
        "http://workspace.example",
        "http://192.168.1.1",
    ] {
        let mut candidate = binding();
        candidate.server_origin = origin.into();
        assert_eq!(
            store.activate_registration(&candidate).await,
            Err(StoreError::InvalidRegistration),
            "{origin}"
        );
    }
    let mut invalid_variant = binding();
    invalid_variant.workspace_id = "00000000-0000-4000-0000-000000000000".into();
    assert_eq!(
        store.activate_registration(&invalid_variant).await,
        Err(StoreError::InvalidRegistration)
    );
    let mut loopback = binding();
    loopback.server_origin = "http://127.0.0.1:8080".into();
    store.activate_registration(&loopback).await.unwrap();
    store.close().await.unwrap();
}

#[tokio::test]
async fn frozen_draft_can_be_edited_only_through_a_new_recovery_session() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("drafts.db");
    let first = binding();
    let second = binding();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&first).await.unwrap();
    let old = store
        .open_registered_document(&first, "doc", b"original base", "rev-a")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(&first, "doc", &old.edit_session_id, 1, b"draft")
        .await
        .unwrap();
    store
        .close_registered_document(&first, "doc", &old.edit_session_id)
        .await
        .unwrap();
    store.activate_registration(&second).await.unwrap();
    let recovered = store.open_recovery_draft(&first, "doc").await.unwrap();
    assert_eq!(recovered.bytes, b"draft");
    assert_ne!(recovered.edit_session_id, old.edit_session_id);
    assert_eq!(
        store
            .save_recovery_checkpoint(&first, "doc", &old.edit_session_id, 2, b"stale")
            .await,
        Err(StoreError::StaleSession)
    );
    store
        .save_recovery_checkpoint(
            &first,
            "doc",
            &recovered.edit_session_id,
            2,
            b"recovery edit",
        )
        .await
        .unwrap();
    store
        .close_recovery_draft(&first, "doc", &recovered.edit_session_id)
        .await
        .unwrap();
    let (reopened, active) = store
        .try_open_registered_draft(&first, "doc")
        .await
        .unwrap()
        .unwrap();
    assert!(!active);
    assert_eq!(reopened.bytes, b"recovery edit");
    assert_eq!(reopened.generation, 2);
    assert_eq!(
        store.try_open_registered_draft(&first, "doc").await,
        Err(StoreError::SessionAlreadyOpen)
    );
    store
        .close_recovery_draft(&first, "doc", &reopened.edit_session_id)
        .await
        .unwrap();
    assert_eq!(
        store.open_recovery_draft(&first, "missing").await,
        Err(StoreError::MissingDraft)
    );
    assert_eq!(
        store.open_recovery_draft(&second, "doc").await,
        Err(StoreError::StaleRegistration)
    );
    store.close().await.unwrap();
    let mut reopened = ClientStore::open(&path).await.unwrap();
    let draft = reopened
        .read_registered_checkpoint(&first, "doc")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.generation, 2);
    assert_eq!(draft.bytes, b"recovery edit");
    assert_eq!(draft.base_bytes, b"original base");
    assert_eq!(draft.base_revision, "rev-a");
    reopened.close().await.unwrap();
}

async fn legacy_database(path: &std::path::Path, migration_collision: bool) {
    let mut connection = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true),
    )
    .await
    .unwrap();
    sqlx::raw_sql("CREATE TABLE draft_checkpoints (document_id TEXT PRIMARY KEY NOT NULL, generation INTEGER NOT NULL CHECK (generation > 0), format_version INTEGER NOT NULL CHECK (format_version = 1), bytes BLOB NOT NULL CHECK (length(bytes) <= 5242880)); CREATE TABLE draft_bases (document_id TEXT PRIMARY KEY NOT NULL REFERENCES draft_checkpoints(document_id), bytes BLOB NOT NULL CHECK (length(bytes) <= 5242880), revision TEXT NOT NULL); INSERT INTO draft_checkpoints VALUES ('welcome', 7, 1, X'6472616674'); INSERT INTO draft_bases VALUES ('welcome', X'62617365', 'legacy-rev'); PRAGMA user_version=1; PRAGMA application_id=1314148952;").execute(&mut connection).await.unwrap();
    if migration_collision {
        sqlx::raw_sql("CREATE TABLE client_registrations (sentinel BLOB); INSERT INTO client_registrations VALUES (X'010203');").execute(&mut connection).await.unwrap();
    }
    connection.close().await.unwrap();
}

#[tokio::test]
async fn migration_keeps_legacy_draft_and_base_accessible() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("legacy.db");
    legacy_database(&path, false).await;
    let mut store = ClientStore::open(&path).await.unwrap();
    let legacy = store.read_checkpoint("welcome").await.unwrap().unwrap();
    assert_eq!(legacy.generation, 7);
    assert_eq!(legacy.bytes, b"draft");
    assert_eq!(legacy.base_bytes, b"base");
    assert_eq!(legacy.base_revision, "legacy-rev");
    store.activate_registration(&binding()).await.unwrap();
    store.close().await.unwrap();
    let mut reopened = ClientStore::open(&path).await.unwrap();
    assert_eq!(
        reopened
            .read_checkpoint("welcome")
            .await
            .unwrap()
            .unwrap()
            .bytes,
        b"draft"
    );
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn failed_migration_preserves_old_schema_and_draft() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("legacy.db");
    legacy_database(&path, true).await;
    assert!(matches!(
        ClientStore::open(&path).await,
        Err(StoreError::Storage)
    ));
    let mut connection = SqliteConnection::connect_with(
        &SqliteConnectOptions::new().filename(&path).read_only(true),
    )
    .await
    .unwrap();
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut connection)
        .await
        .unwrap();
    let draft: Vec<u8> =
        sqlx::query_scalar("SELECT bytes FROM draft_checkpoints WHERE document_id = 'welcome'")
            .fetch_one(&mut connection)
            .await
            .unwrap();
    let sentinel: Vec<u8> = sqlx::query_scalar("SELECT sentinel FROM client_registrations")
        .fetch_one(&mut connection)
        .await
        .unwrap();
    assert_eq!(version, 1);
    assert_eq!(draft, b"draft");
    assert_eq!(sentinel, b"\x01\x02\x03");
    connection.close().await.unwrap();
}
