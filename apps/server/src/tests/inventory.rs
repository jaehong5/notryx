use super::*;
use notryx_protocol::NodeKind;

fn create_request(rig: &Rig, name: &str, kind: NodeKind, sequence: u64) -> CreateNode {
    CreateNode {
        upload_id: None,
        epoch: rig.workspace.epoch(),
        stream_id: rig.stream,
        operation_seq: sequence,
        operation_id: op_id(),
        node_id: node_id(),
        parent_id: rig.workspace.state.root_id,
        name: name.into(),
        kind,
    }
}

fn legacy_saved_tag(document: &Document) -> String {
    let quoted = |value: &str| serde_json::to_string(value).unwrap();
    let bytes = format!(
        "{{\"node_id\":{},\"parent_id\":{},\"name\":{},\"content_rev\":{},\"meta_rev\":{},\"content_hash\":{},\"size\":{},\"last_change_seq\":{}}}",
        quoted(&document.node_id.to_string()),
        quoted(&document.parent_id.to_string()),
        quoted(&document.name),
        quoted(document.content_rev.as_str()),
        quoted(document.meta_rev.as_str()),
        quoted(&document.content_hash),
        document.size,
        quoted(&document.last_change_seq.get().to_string())
    );
    format!("\"n.{}\"", digest(bytes.as_bytes()))
}

fn legacy_created_file_tag(node: &NodeSnapshot) -> String {
    let quoted = |value: &str| serde_json::to_string(value).unwrap();
    let bytes = format!(
        "{{\"node_id\":{},\"parent_id\":{},\"name\":{},\"kind\":\"file\",\"content_rev\":{},\"meta_rev\":{},\"content_hash\":{},\"size\":{},\"children_generation\":{},\"last_change_seq\":{}}}",
        quoted(&node.node_id.to_string()),
        quoted(&node.parent_id.unwrap().to_string()),
        quoted(&node.name),
        quoted(node.content_rev.as_ref().unwrap().as_str()),
        quoted(node.meta_rev.as_str()),
        quoted(node.content_hash.as_ref().unwrap()),
        node.size.unwrap(),
        node.children_generation,
        quoted(&node.last_change_seq.get().to_string())
    );
    format!("\"n.{}\"", digest(bytes.as_bytes()))
}

#[tokio::test]
async fn inventory_returns_root_and_nodes_in_exclusive_uuid_order() {
    let mut rig = Rig::new().await;
    let mut first = create_request(&rig, "First.md", NodeKind::File, 1);
    first.node_id = "00000000-0001-7000-8000-000000000001".parse().unwrap();
    let mut last = create_request(&rig, "Last", NodeKind::Folder, 2);
    last.node_id = "ffffffff-ffff-7fff-bfff-ffffffffffff".parse().unwrap();
    rig.workspace.create_node(first.clone()).await.unwrap();
    rig.workspace.create_node(last.clone()).await.unwrap();
    let mut expected = vec![
        first.node_id,
        last.node_id,
        rig.initial.node_id,
        rig.workspace.state.root_id,
    ];
    expected.sort_by_key(|id| id.to_string());
    let mut actual = Vec::new();
    let mut after = None;
    loop {
        let page = rig.workspace.inventory(after, 1).await.unwrap();
        assert_eq!(page.workspace_epoch, rig.workspace.epoch());
        assert_eq!(page.nodes.len(), 1);
        let node = &page.nodes[0];
        assert_eq!(*node, rig.workspace.node_state(node.node_id).await.unwrap());
        actual.push(node.node_id);
        if let Some(next) = page.next_after_id {
            assert_eq!(next, node.node_id);
            after = Some(next);
        } else {
            break;
        }
    }
    assert_eq!(actual, expected);
    let page = rig
        .workspace
        .inventory(Some(last.node_id), 100)
        .await
        .unwrap();
    assert!(page.nodes.is_empty());
    assert_eq!(page.next_after_id, None);
    let nonexistent = "ffffffff-ffff-7fff-bfff-fffffffffffe".parse().unwrap();
    let page = rig
        .workspace
        .inventory(Some(nonexistent), 100)
        .await
        .unwrap();
    assert_eq!(page.nodes.len(), 1);
    assert_eq!(page.nodes[0].node_id, last.node_id);
}

#[tokio::test]
async fn full_node_and_inventory_read_committed_metadata_without_content_objects() {
    let mut rig = Rig::new().await;
    let root = rig.workspace.state.root_id;
    let root_state = rig.workspace.node_state(root).await.unwrap();
    assert_eq!(root_state.parent_id, None);
    assert_eq!(root_state.name, "");
    assert_eq!(root_state.kind, NodeKind::Folder);
    assert_eq!(root_state.content_rev, None);
    assert_eq!(root_state.content_etag, None);
    assert_eq!(root_state.content_hash, None);
    assert_eq!(root_state.size, None);
    assert!(!root_state.deleted);
    let document = rig.workspace.node_state(rig.initial.node_id).await.unwrap();
    assert_eq!(
        document.content_hash,
        Some(format!("sha256:{}", rig.initial.content_hash))
    );
    assert_eq!(document.content_rev, Some(rig.initial.content_rev.clone()));
    let objects = rig.dir.path().join("state/objects/sha256");
    for entry in fs::read_dir(objects).unwrap() {
        fs::remove_file(entry.unwrap().path()).unwrap();
    }
    rig.workspace.ready = false;
    assert_eq!(
        rig.workspace.node_state(rig.initial.node_id).await.unwrap(),
        document
    );
    assert_eq!(
        rig.workspace
            .inventory(None, 100)
            .await
            .unwrap()
            .nodes
            .len(),
        2
    );
    assert_eq!(
        rig.workspace.node_state(node_id()).await,
        Err(Error::NotFound)
    );
    assert!(!rig.workspace.ready());
}

#[tokio::test]
async fn inventory_is_a_live_keyset_and_rejects_invalid_limits() {
    let mut rig = Rig::new().await;
    for limit in [0, 101, u16::MAX] {
        assert_eq!(
            rig.workspace.inventory(None, limit).await,
            Err(Error::InvalidRequest)
        );
    }
    let first = rig.workspace.inventory(None, 1).await.unwrap();
    let cursor = first.next_after_id.unwrap();
    let mut added = create_request(&rig, "Later.md", NodeKind::File, 1);
    added.node_id = "ffffffff-ffff-7fff-bfff-ffffffffffff".parse().unwrap();
    rig.workspace.create_node(added.clone()).await.unwrap();
    let second = rig.workspace.inventory(Some(cursor), 100).await.unwrap();
    assert!(
        second
            .nodes
            .iter()
            .any(|node| node.node_id == added.node_id)
    );
    assert!(
        second
            .nodes
            .iter()
            .all(|node| node.node_id.to_string() > cursor.to_string())
    );
}

#[tokio::test]
async fn inventory_caps_the_serialized_envelope_without_skipping_nodes() {
    let mut rig = Rig::new().await;
    sqlx::query("UPDATE nodes SET meta_rev=?")
        .bind("x".repeat(300 * 1024))
        .execute(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    let page = rig.workspace.inventory(None, 100).await.unwrap();
    assert_eq!(page.nodes.len(), 1);
    assert_eq!(page.next_after_id, Some(page.nodes[0].node_id));
    assert!(serde_json::to_vec(&page).unwrap().len() <= 1024 * 1024);
    let next = rig
        .workspace
        .inventory(page.next_after_id, 100)
        .await
        .unwrap();
    assert_eq!(next.nodes.len(), 1);
    assert_ne!(page.nodes[0].node_id, next.nodes[0].node_id);
    assert_eq!(next.next_after_id, None);
    assert!(serde_json::to_vec(&next).unwrap().len() <= 1024 * 1024);
    sqlx::query("UPDATE nodes SET meta_rev=?")
        .bind("x".repeat(600 * 1024))
        .execute(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    assert_eq!(
        rig.workspace.inventory(None, 100).await,
        Err(Error::InvalidFormat)
    );
}

#[tokio::test]
async fn new_store_requires_schema_seven_for_tombstones() {
    let mut rig = Rig::new().await;
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    assert_eq!(version, 10);
}

#[tokio::test]
async fn successful_receipts_and_own_state_share_one_validator() {
    let mut rig = Rig::new().await;
    let root = rig.workspace.state.root_id;
    let before = rig.workspace.node_state(root).await.unwrap();
    let create = create_request(&rig, "New.md", NodeKind::File, 1);
    rig.workspace.create_node(create.clone()).await.unwrap();
    let created = rig.workspace.node_state(create.node_id).await.unwrap();
    let after = rig.workspace.node_state(root).await.unwrap();
    assert_eq!(before.meta_etag, after.meta_etag);
    assert_ne!(before.node_etag, after.node_etag);
    let saved_request = SaveContent {
        epoch: rig.workspace.epoch(),
        stream_id: rig.stream,
        operation_seq: 2,
        operation_id: op_id(),
        node_id: create.node_id,
        expected_content_rev: created.content_rev.clone().unwrap(),
        content_hash: digest(b"changed\n"),
        bytes: b"changed\n".to_vec(),
    };
    rig.workspace.save(saved_request.clone()).await.unwrap();
    let saved = rig.workspace.node_state(create.node_id).await.unwrap();
    assert_ne!(created.node_etag, saved.node_etag);
    for (operation_id, expected) in [
        (create.operation_id, created.node_etag),
        (saved_request.operation_id, saved.node_etag.clone()),
    ] {
        let tag: Option<String> =
            sqlx::query_scalar("SELECT node_etag FROM operation_receipts WHERE operation_id=?")
                .bind(operation_id.to_string())
                .fetch_one(&mut rig.workspace.state.connection)
                .await
                .unwrap();
        assert_eq!(tag, Some(expected));
    }
    rig.workspace.close().await.unwrap();
    let mut reopened =
        Workspace::open(&rig.dir.path().join("notes"), &rig.dir.path().join("state"))
            .await
            .unwrap();
    assert_eq!(reopened.node_state(create.node_id).await.unwrap(), saved);
}

async fn downgrade_to_four(connection: &mut sqlx::SqliteConnection) {
    super::legacy::nodes_before_deletion(connection).await;
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pragma_table_info('operation_receipts') WHERE name='node_etag'",
    )
    .fetch_one(&mut *connection)
    .await
    .unwrap();
    if count == 1 {
        sqlx::query("ALTER TABLE operation_receipts DROP COLUMN node_etag")
            .execute(&mut *connection)
            .await
            .unwrap();
    }
    sqlx::query("PRAGMA user_version=4")
        .execute(connection)
        .await
        .unwrap();
}

#[tokio::test]
async fn migration_preserves_legacy_success_validators_and_current_metadata() {
    let mut rig = Rig::new().await;
    let saved_request = rig.command(b"legacy saved bytes\r\n");
    let saved = rig.workspace.save(saved_request.clone()).await.unwrap();
    let Outcome::Saved(document) = &saved.outcome else {
        panic!("saved")
    };
    let legacy_saved = legacy_saved_tag(document);
    let create = create_request(&rig, "Created.md", NodeKind::File, 2);
    let created = rig.workspace.create_node(create.clone()).await.unwrap();
    let Outcome::Created(node) = &created.outcome else {
        panic!("created")
    };
    let legacy_created = legacy_created_file_tag(node);
    let before_head = rig.workspace.server_info().await.unwrap().head_seq;
    let before_epoch = rig.workspace.epoch();
    downgrade_to_four(&mut rig.workspace.state.connection).await;
    rig.workspace.close().await.unwrap();
    let notes = rig.dir.path().join("notes");
    let state = rig.dir.path().join("state");
    assert!(matches!(
        Workspace::open(&notes, &state).await,
        Err(Error::MigrationRequired)
    ));
    Workspace::migrate(&notes, &state).await.unwrap();
    Workspace::migrate(&notes, &state).await.unwrap();
    let mut reopened = Workspace::open(&notes, &state).await.unwrap();
    assert_eq!(reopened.epoch(), before_epoch);
    assert_eq!(reopened.server_info().await.unwrap().head_seq, before_head);
    assert_eq!(reopened.save(saved_request.clone()).await.unwrap(), saved);
    assert_eq!(reopened.create_node(create.clone()).await.unwrap(), created);
    for (operation_id, expected) in [
        (saved_request.operation_id, legacy_saved),
        (create.operation_id, legacy_created),
    ] {
        let tag: Option<String> =
            sqlx::query_scalar("SELECT node_etag FROM operation_receipts WHERE operation_id=?")
                .bind(operation_id.to_string())
                .fetch_one(&mut reopened.state.connection)
                .await
                .unwrap();
        assert_eq!(tag, Some(expected));
    }
    assert_eq!(
        fs::read(notes.join("Document.md")).unwrap(),
        b"legacy saved bytes\r\n"
    );
}

#[tokio::test]
async fn migration_failure_rolls_back_new_column_and_preserves_legacy_receipts() {
    let mut rig = Rig::new().await;
    let request = rig.command(b"saved\n");
    rig.workspace.save(request.clone()).await.unwrap();
    downgrade_to_four(&mut rig.workspace.state.connection).await;
    let outcome: String = sqlx::query_scalar("SELECT outcome FROM operation_receipts")
        .fetch_one(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    sqlx::raw_sql("CREATE TRIGGER fail_receipt_backfill BEFORE UPDATE ON operation_receipts BEGIN SELECT RAISE(ABORT, 'synthetic failure'); END;")
        .execute(&mut rig.workspace.state.connection).await.unwrap();
    rig.workspace.close().await.unwrap();
    let notes = rig.dir.path().join("notes");
    let state = rig.dir.path().join("state");
    assert!(Workspace::migrate(&notes, &state).await.is_err());
    let mut db = sqlx::SqliteConnection::connect_with(
        &sqlx::sqlite::SqliteConnectOptions::new().filename(state.join("state.db")),
    )
    .await
    .unwrap();
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut db)
        .await
        .unwrap();
    let column: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pragma_table_info('operation_receipts') WHERE name='node_etag'",
    )
    .fetch_one(&mut db)
    .await
    .unwrap();
    let preserved: String = sqlx::query_scalar("SELECT outcome FROM operation_receipts")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(version, 4);
    assert_eq!(column, 0);
    assert_eq!(preserved, outcome);
    assert_eq!(fs::read(notes.join("Document.md")).unwrap(), b"saved\n");
    db.close().await.unwrap();
}

#[tokio::test]
async fn pending_save_and_create_survive_migration_and_finish_with_current_validators() {
    for creating in [false, true] {
        let mut rig = Rig::new().await;
        let create = create_request(&rig, "Pending.md", NodeKind::File, 1);
        let save = rig.command(b"pending save\n");
        let (operation_id, id) = if creating {
            (create.operation_id, create.node_id)
        } else {
            (save.operation_id, save.node_id)
        };
        rig.workspace.hook = Some(Box::new(|phase| {
            if phase == Phase::BeforeFinish {
                Err(Error::Storage)
            } else {
                Ok(())
            }
        }));
        let result = if creating {
            rig.workspace.create_node(create).await
        } else {
            rig.workspace.save(save).await
        };
        assert_eq!(result, Err(Error::Storage));
        let intent: String = sqlx::query_scalar("SELECT intent FROM write_intents")
            .fetch_one(&mut rig.workspace.state.connection)
            .await
            .unwrap();
        downgrade_to_four(&mut rig.workspace.state.connection).await;
        rig.workspace.close().await.unwrap();
        let notes = rig.dir.path().join("notes");
        let state = rig.dir.path().join("state");
        Workspace::migrate(&notes, &state).await.unwrap();
        let mut loaded = Workspace::load(&notes, &state).await.unwrap();
        let preserved: String = sqlx::query_scalar("SELECT intent FROM write_intents")
            .fetch_one(&mut loaded.state.connection)
            .await
            .unwrap();
        assert_eq!(preserved, intent);
        let tag: Option<String> = sqlx::query_scalar("SELECT node_etag FROM operation_receipts")
            .fetch_one(&mut loaded.state.connection)
            .await
            .unwrap();
        assert_eq!(tag, None);
        loaded.recover().await.unwrap();
        let expected = loaded.node_state(id).await.unwrap().node_etag;
        let tag: Option<String> =
            sqlx::query_scalar("SELECT node_etag FROM operation_receipts WHERE operation_id=?")
                .bind(operation_id.to_string())
                .fetch_one(&mut loaded.state.connection)
                .await
                .unwrap();
        assert_eq!(tag, Some(expected));
        assert!(loaded.ready());
    }
}

#[tokio::test]
async fn startup_rejects_missing_corrupt_and_inapplicable_receipt_validators() {
    for corruption in [
        "UPDATE operation_receipts SET node_etag=NULL",
        "UPDATE operation_receipts SET node_etag='\"n.invalid\"'",
        "UPDATE operation_receipts SET outcome='\"Abandoned\"'",
        "ALTER TABLE operation_receipts DROP COLUMN node_etag",
    ] {
        let mut rig = Rig::new().await;
        rig.workspace
            .save(rig.command(b"saved evidence\n"))
            .await
            .unwrap();
        sqlx::raw_sql(corruption)
            .execute(&mut rig.workspace.state.connection)
            .await
            .unwrap();
        rig.workspace.close().await.unwrap();
        let notes = rig.dir.path().join("notes");
        let state = rig.dir.path().join("state");
        let before = fs::read(state.join("state.db")).unwrap();
        assert!(matches!(
            Workspace::open(&notes, &state).await,
            Err(Error::InvalidFormat)
        ));
        assert_eq!(fs::read(state.join("state.db")).unwrap(), before);
        assert_eq!(
            fs::read(notes.join("Document.md")).unwrap(),
            b"saved evidence\n"
        );
    }
}
