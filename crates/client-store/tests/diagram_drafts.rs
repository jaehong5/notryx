use notryx_client_store::{ClientStore, DiagramDraftCheckpoint, RegistrationBinding, StoreError};
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use uuid::Uuid;

const NODE: &str = "019a1000-0000-7000-8000-000000000002";
const BASE: &str = "<mxGraphModel><root/></mxGraphModel>";

fn binding() -> RegistrationBinding {
    RegistrationBinding {
        server_origin: "https://workspace.example".into(),
        workspace_id: Uuid::new_v4().to_string(),
        workspace_epoch: Uuid::new_v4().to_string(),
        device_id: Uuid::new_v4().to_string(),
        device_stream_id: Uuid::new_v4().to_string(),
    }
}

fn checkpoint(expected_generation: u64, xml: &str) -> DiagramDraftCheckpoint {
    DiagramDraftCheckpoint {
        expected_generation,
        xml: xml.into(),
        base_xml: BASE.into(),
        base_revision: "original-revision".into(),
    }
}

async fn connect(path: &std::path::Path) -> SqliteConnection {
    SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(path))
        .await
        .unwrap()
}

#[tokio::test]
async fn checkpoints_survive_reopen_and_preserve_the_original_base() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("client.sqlite");
    let registration = binding();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&registration).await.unwrap();
    assert_eq!(
        store.read_diagram_draft(&registration, NODE).await.unwrap(),
        None
    );
    let xml = "<mxGraphModel><root><mxCell value=\"한글\"/></root></mxGraphModel>";
    assert_eq!(
        store
            .save_diagram_draft(&registration, NODE, &checkpoint(0, xml))
            .await,
        Ok(1)
    );
    assert!(store.list_registered_drafts().await.unwrap().is_empty());
    store.close().await.unwrap();

    let mut store = ClientStore::open(&path).await.unwrap();
    let restored = store
        .read_diagram_draft(&registration, NODE)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            restored.generation,
            restored.xml.as_str(),
            restored.base_xml.as_str(),
            restored.base_revision.as_str()
        ),
        (1, xml, BASE, "original-revision")
    );
    assert_eq!(
        store
            .save_diagram_draft(&registration, NODE, &checkpoint(0, BASE))
            .await,
        Err(StoreError::StaleGeneration)
    );
    let mut changed_base = checkpoint(1, xml);
    changed_base.base_xml = "<mxfile/>".into();
    changed_base.base_revision = "new-server-revision".into();
    assert_eq!(
        store
            .save_diagram_draft(&registration, NODE, &changed_base)
            .await,
        Err(StoreError::InvalidRequest)
    );
    assert_eq!(
        store
            .read_diagram_draft(&registration, NODE)
            .await
            .unwrap()
            .unwrap(),
        restored
    );
    assert_eq!(
        store
            .save_diagram_draft(&registration, NODE, &checkpoint(1, BASE))
            .await,
        Ok(2)
    );
    let next = store
        .read_diagram_draft(&registration, NODE)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            next.generation,
            next.base_xml.as_str(),
            next.base_revision.as_str()
        ),
        (2, BASE, "original-revision")
    );
}

#[tokio::test]
async fn registration_replacement_isolates_but_does_not_delete_existing_diagrams() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("client.sqlite");
    let old = binding();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&old).await.unwrap();
    store
        .save_diagram_draft(&old, NODE, &checkpoint(0, BASE))
        .await
        .unwrap();
    let next = RegistrationBinding {
        workspace_epoch: Uuid::new_v4().to_string(),
        ..old.clone()
    };
    store.activate_registration(&next).await.unwrap();
    assert_eq!(
        store.read_diagram_draft(&old, NODE).await,
        Err(StoreError::StaleRegistration)
    );
    assert_eq!(
        store
            .save_diagram_draft(&old, NODE, &checkpoint(1, BASE))
            .await,
        Err(StoreError::StaleRegistration)
    );
    assert_eq!(store.read_diagram_draft(&next, NODE).await.unwrap(), None);
    assert_eq!(
        store
            .save_diagram_draft(&next, NODE, &checkpoint(0, "<mxfile/>"))
            .await,
        Ok(1)
    );
    store.close().await.unwrap();
    let mut db = connect(&path).await;
    let retained: String = sqlx::query_scalar("SELECT d.xml FROM diagram_drafts d JOIN client_registrations r ON r.id=d.registration_id WHERE r.workspace_epoch=?")
        .bind(&old.workspace_epoch).fetch_one(&mut db).await.unwrap();
    assert_eq!(retained, BASE);
}

#[tokio::test]
async fn v24_upgrade_preserves_existing_drafts_and_credential_expiry() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("client.sqlite");
    let registration = binding();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&registration).await.unwrap();
    store
        .set_credential_expiry(&registration, 100, 300)
        .await
        .unwrap();
    let document = store
        .open_registered_document(&registration, NODE, b"markdown base", "markdown-revision")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(
            &registration,
            NODE,
            &document.edit_session_id,
            1,
            b"markdown draft",
        )
        .await
        .unwrap();
    store.close().await.unwrap();
    let mut db = connect(&path).await;
    sqlx::raw_sql("DROP TABLE IF EXISTS diagram_drafts; PRAGMA user_version=24;")
        .execute(&mut db)
        .await
        .unwrap();
    sqlx::raw_sql("CREATE TABLE diagram_drafts(sentinel TEXT); INSERT INTO diagram_drafts VALUES('preserve-me');")
        .execute(&mut db).await.unwrap();
    db.close().await.unwrap();
    assert!(ClientStore::open(&path).await.is_err());
    let mut db = connect(&path).await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("PRAGMA user_version")
            .fetch_one(&mut db)
            .await
            .unwrap(),
        24
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT sentinel FROM diagram_drafts")
            .fetch_one(&mut db)
            .await
            .unwrap(),
        "preserve-me"
    );
    sqlx::query("DROP TABLE diagram_drafts")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    assert_eq!(
        store.credential_expiry(&registration).await.unwrap(),
        Some((100, 300))
    );
    assert_eq!(
        store
            .read_registered_checkpoint(&registration, NODE)
            .await
            .unwrap()
            .unwrap()
            .bytes,
        b"markdown draft"
    );
    assert_eq!(
        store
            .save_diagram_draft(&registration, NODE, &checkpoint(0, BASE))
            .await,
        Ok(1)
    );
    store.close().await.unwrap();
    let mut db = connect(&path).await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("PRAGMA user_version")
            .fetch_one(&mut db)
            .await
            .unwrap(),
        25
    );
}

#[tokio::test]
async fn invalid_checkpoints_leave_the_previous_draft_intact() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("client.sqlite");
    let registration = binding();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&registration).await.unwrap();
    store
        .save_diagram_draft(&registration, NODE, &checkpoint(0, BASE))
        .await
        .unwrap();
    let original = store.read_diagram_draft(&registration, NODE).await.unwrap();
    for input in [
        checkpoint(1, ""),
        checkpoint(1, &"x".repeat(5 * 1024 * 1024 + 1)),
        checkpoint(u64::MAX, BASE),
    ] {
        assert!(
            store
                .save_diagram_draft(&registration, NODE, &input)
                .await
                .is_err()
        );
    }
    assert!(
        store
            .save_diagram_draft(&registration, "bad-node", &checkpoint(0, BASE))
            .await
            .is_err()
    );
    assert_eq!(
        store.read_diagram_draft(&registration, NODE).await.unwrap(),
        original
    );
}
