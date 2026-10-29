use super::*;
use notryx_server_storage::StorageStage;

#[tokio::test]
async fn external_file_change_commits_journal_and_preserves_bytes() {
    let mut rig = Rig::new().await;
    let path = rig.dir.path().join("notes/Document.md");
    let old_bytes = fs::read(&path).unwrap();
    let new_bytes = b"\xef\xbb\xbf# External\r\n\r\n<!-- preserved -->\r\n";
    fs::write(&path, new_bytes).unwrap();

    let probe = rig
        .workspace
        .prepare_external_file(rig.initial.node_id)
        .await
        .unwrap();
    let candidate = probe.observe().unwrap();
    let changed = rig
        .workspace
        .commit_external_file(candidate)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(changed.content_rev, rig.initial.content_rev);
    assert_eq!(changed.meta_rev, rig.initial.meta_rev);
    assert_eq!(changed.parent_id, rig.initial.parent_id);
    assert_eq!(
        changed.last_change_seq.get(),
        rig.initial.last_change_seq.get() + 1
    );
    assert_eq!(
        rig.workspace
            .files
            .read_object(&rig.initial.content_hash)
            .unwrap(),
        old_bytes
    );
    assert_eq!(
        rig.workspace.read(changed.node_id).await.unwrap().1,
        new_bytes
    );
    let page = rig
        .workspace
        .changes(rig.initial.last_change_seq, None)
        .await
        .unwrap();
    assert_eq!(page.covered_through, changed.last_change_seq);
    assert_eq!(page.nodes.len(), 1);
    assert_eq!(page.nodes[0].node_id, changed.node_id);
    assert_eq!(
        page.nodes[0].content_rev.as_ref(),
        Some(&changed.content_rev)
    );

    let request = rig.command(b"local request\n");
    assert_eq!(
        rig.workspace.save(request).await.unwrap().outcome,
        Outcome::PreconditionFailed {
            current: changed.content_rev.clone()
        }
    );
    assert_eq!(fs::read(path).unwrap(), new_bytes);
    rig.workspace.close().await.unwrap();
    let mut reopened =
        Workspace::open(&rig.dir.path().join("notes"), &rig.dir.path().join("state"))
            .await
            .unwrap();
    assert_eq!(reopened.read(changed.node_id).await.unwrap().1, new_bytes);
}

#[tokio::test]
async fn stale_external_candidate_never_overwrites_newer_file() {
    let mut rig = Rig::new().await;
    let path = rig.dir.path().join("notes/Document.md");
    fs::write(&path, b"first external state\n").unwrap();
    let candidate = rig
        .workspace
        .prepare_external_file(rig.initial.node_id)
        .await
        .unwrap()
        .observe()
        .unwrap();
    fs::write(&path, b"newer external state\n").unwrap();
    assert_eq!(
        rig.workspace.commit_external_file(candidate).await,
        Err(Error::ExternalChange)
    );
    assert_eq!(
        rig.workspace.state.head_seq().await.unwrap(),
        rig.initial.last_change_seq
    );
    assert_eq!(
        rig.workspace.read(rig.initial.node_id).await.unwrap().0,
        rig.initial
    );
    assert_eq!(fs::read(path).unwrap(), b"newer external state\n");
}

#[tokio::test]
async fn external_sync_failure_preserves_existing_metadata_and_source() {
    let mut rig = Rig::new().await;
    let path = rig.dir.path().join("notes/Document.md");
    fs::write(&path, b"external content\n").unwrap();
    let candidate = rig
        .workspace
        .prepare_external_file(rig.initial.node_id)
        .await
        .unwrap()
        .observe_with_storage_hook(|stage| {
            if stage == StorageStage::BeforeRecoveryFileSync {
                return Err(Error::Storage);
            }
            Ok(())
        })
        .unwrap();
    assert_eq!(
        rig.workspace.commit_external_file(candidate).await,
        Err(Error::Storage)
    );
    assert_eq!(
        rig.workspace.state.head_seq().await.unwrap(),
        rig.initial.last_change_seq
    );
    assert_eq!(
        rig.workspace.read(rig.initial.node_id).await.unwrap().0,
        rig.initial
    );
    assert_eq!(fs::read(path).unwrap(), b"external content\n");
}

#[tokio::test]
async fn external_sqlite_failure_cannot_publish_partial_journal() {
    let mut rig = Rig::new().await;
    let path = rig.dir.path().join("notes/Document.md");
    fs::write(&path, b"external content\n").unwrap();
    let candidate = rig
        .workspace
        .prepare_external_file(rig.initial.node_id)
        .await
        .unwrap()
        .observe()
        .unwrap();
    sqlx::raw_sql("CREATE TRIGGER fail_external_journal BEFORE INSERT ON node_changes BEGIN SELECT RAISE(FAIL,'injected'); END;")
        .execute(&mut rig.workspace.state.connection).await.unwrap();
    assert_eq!(
        rig.workspace.commit_external_file(candidate).await,
        Err(Error::Storage)
    );
    assert_eq!(
        rig.workspace.state.head_seq().await.unwrap(),
        rig.initial.last_change_seq
    );
    assert_eq!(
        rig.workspace.read(rig.initial.node_id).await.unwrap().0,
        rig.initial
    );
    assert_eq!(fs::read(&path).unwrap(), b"external content\n");
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM change_sets")
        .fetch_one(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    assert_eq!(count, 1);
    rig.workspace.close().await.unwrap();
    let mut reopened =
        Workspace::open(&rig.dir.path().join("notes"), &rig.dir.path().join("state"))
            .await
            .unwrap();
    assert_eq!(
        reopened.read(rig.initial.node_id).await.unwrap().1,
        fs::read(
            rig.dir
                .path()
                .join("state/objects/sha256")
                .join(&rig.initial.content_hash)
        )
        .unwrap()
    );
}

#[tokio::test]
async fn absent_source_is_never_a_delete_reconcile() {
    let mut rig = Rig::new().await;
    fs::remove_file(rig.dir.path().join("notes/Document.md")).unwrap();
    let result = rig
        .workspace
        .prepare_external_file(rig.initial.node_id)
        .await
        .unwrap()
        .observe();
    assert!(matches!(result, Err(Error::NotFound)));
    assert_eq!(
        rig.workspace.state.head_seq().await.unwrap(),
        rig.initial.last_change_seq
    );
    assert!(
        !rig.workspace
            .state
            .node(rig.initial.node_id)
            .await
            .unwrap()
            .unwrap()
            .deleted
    );
}

#[tokio::test]
async fn corrupt_previous_object_fails_closed_without_ingesting_external_bytes() {
    use std::os::unix::fs::PermissionsExt;
    let mut rig = Rig::new().await;
    let object = rig
        .dir
        .path()
        .join("state/objects/sha256")
        .join(&rig.initial.content_hash);
    fs::set_permissions(&object, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(&object, b"corrupt old object").unwrap();
    fs::set_permissions(&object, fs::Permissions::from_mode(0o400)).unwrap();
    let path = rig.dir.path().join("notes/Document.md");
    fs::write(&path, b"external content\n").unwrap();
    let candidate = rig
        .workspace
        .prepare_external_file(rig.initial.node_id)
        .await
        .unwrap()
        .observe()
        .unwrap();
    assert_eq!(
        rig.workspace.commit_external_file(candidate).await,
        Err(Error::InvalidFormat)
    );
    assert!(!rig.workspace.ready());
    assert_eq!(
        rig.workspace.state.head_seq().await.unwrap(),
        rig.initial.last_change_seq
    );
    assert_eq!(fs::read(path).unwrap(), b"external content\n");
}

#[tokio::test]
async fn corrupt_proposed_object_fails_closed_and_preserves_external_source() {
    let mut rig = Rig::new().await;
    let path = rig.dir.path().join("notes/Document.md");
    let external = b"external content\n";
    fs::write(&path, external).unwrap();
    let object = rig
        .dir
        .path()
        .join("state/objects/sha256")
        .join(digest(external));
    fs::write(&object, b"corrupt preexisting object").unwrap();
    let candidate = rig
        .workspace
        .prepare_external_file(rig.initial.node_id)
        .await
        .unwrap()
        .observe()
        .unwrap();
    assert_eq!(
        rig.workspace.commit_external_file(candidate).await,
        Err(Error::InvalidFormat)
    );
    assert!(!rig.workspace.ready());
    assert_eq!(
        rig.workspace.state.head_seq().await.unwrap(),
        rig.initial.last_change_seq
    );
    assert_eq!(fs::read(path).unwrap(), external);
    assert_eq!(fs::read(object).unwrap(), b"corrupt preexisting object");
}

#[tokio::test]
async fn old_object_corruption_is_not_hidden_by_ancestor_move() {
    use notryx_protocol::NodeKind;
    use std::os::unix::fs::PermissionsExt;
    let mut rig = Rig::new().await;
    let root = rig.workspace.state.root_id;
    let folder = CreateNode {
        epoch: rig.workspace.epoch(),
        stream_id: rig.stream,
        operation_seq: 1,
        operation_id: op_id(),
        node_id: node_id(),
        parent_id: root,
        name: "Folder".into(),
        kind: NodeKind::Folder,
        upload_id: None,
    };
    let Outcome::Created(folder) = rig.workspace.create_node(folder).await.unwrap().outcome else {
        panic!("expected folder");
    };
    let node = NodeSnapshot::from(&rig.initial);
    let moved = MoveNode {
        epoch: rig.workspace.epoch(),
        stream_id: rig.stream,
        operation_seq: 2,
        operation_id: op_id(),
        node_id: node.node_id,
        expected_meta_rev: node.meta_rev,
        expected_parent_id: root,
        parent_id: folder.node_id,
        name: "Document.md".into(),
    };
    assert!(matches!(
        rig.workspace.move_node(moved).await.unwrap().outcome,
        Outcome::Moved(_)
    ));

    let object = rig
        .dir
        .path()
        .join("state/objects/sha256")
        .join(&rig.initial.content_hash);
    fs::set_permissions(&object, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(&object, b"corrupt object").unwrap();
    fs::set_permissions(&object, fs::Permissions::from_mode(0o400)).unwrap();
    let path = rig.dir.path().join("notes/Folder/Document.md");
    fs::write(&path, b"external content\n").unwrap();
    let candidate = rig
        .workspace
        .prepare_external_file(rig.initial.node_id)
        .await
        .unwrap()
        .observe()
        .unwrap();
    let move_folder = MoveNode {
        epoch: rig.workspace.epoch(),
        stream_id: rig.stream,
        operation_seq: 3,
        operation_id: op_id(),
        node_id: folder.node_id,
        expected_meta_rev: folder.meta_rev,
        expected_parent_id: root,
        parent_id: root,
        name: "Moved".into(),
    };
    assert!(matches!(
        rig.workspace.move_node(move_folder).await.unwrap().outcome,
        Outcome::Moved(_)
    ));
    let head = rig.workspace.state.head_seq().await.unwrap();
    assert_eq!(
        rig.workspace.commit_external_file(candidate).await,
        Err(Error::InvalidFormat)
    );
    assert!(!rig.workspace.ready());
    assert_eq!(rig.workspace.state.head_seq().await.unwrap(), head);
    assert_eq!(
        fs::read(rig.dir.path().join("notes/Moved/Document.md")).unwrap(),
        b"external content\n"
    );
    assert_eq!(fs::read(object).unwrap(), b"corrupt object");
}

async fn nest_document(rig: &mut Rig) -> NodeSnapshot {
    let root = rig.workspace.state.root_id;
    let folder = CreateNode {
        epoch: rig.workspace.epoch(),
        stream_id: rig.stream,
        operation_seq: 1,
        operation_id: op_id(),
        node_id: node_id(),
        parent_id: root,
        name: "Folder".into(),
        kind: notryx_protocol::NodeKind::Folder,
        upload_id: None,
    };
    let Outcome::Created(folder) = rig.workspace.create_node(folder).await.unwrap().outcome else {
        panic!("expected folder");
    };
    let node = NodeSnapshot::from(&rig.initial);
    let moved = MoveNode {
        epoch: rig.workspace.epoch(),
        stream_id: rig.stream,
        operation_seq: 2,
        operation_id: op_id(),
        node_id: node.node_id,
        expected_meta_rev: node.meta_rev,
        expected_parent_id: root,
        parent_id: folder.node_id,
        name: "Document.md".into(),
    };
    assert!(matches!(
        rig.workspace.move_node(moved).await.unwrap().outcome,
        Outcome::Moved(_)
    ));
    folder
}

async fn move_parent(rig: &mut Rig, folder: NodeSnapshot) {
    let root = rig.workspace.state.root_id;
    let moved = MoveNode {
        epoch: rig.workspace.epoch(),
        stream_id: rig.stream,
        operation_seq: 3,
        operation_id: op_id(),
        node_id: folder.node_id,
        expected_meta_rev: folder.meta_rev,
        expected_parent_id: root,
        parent_id: root,
        name: "Moved".into(),
    };
    assert!(matches!(
        rig.workspace.move_node(moved).await.unwrap().outcome,
        Outcome::Moved(_)
    ));
}

#[tokio::test]
async fn referenced_proposed_corruption_is_not_hidden_by_ancestor_move() {
    use std::os::unix::fs::PermissionsExt;
    let mut rig = Rig::new().await;
    let external = b"shared external content\n";
    fs::write(rig.dir.path().join("notes/Shared.md"), external).unwrap();
    let shared = rig
        .workspace
        .adopt_root_document("Shared.md")
        .await
        .unwrap();
    let folder = nest_document(&mut rig).await;
    let object = rig
        .dir
        .path()
        .join("state/objects/sha256")
        .join(&shared.content_hash);
    fs::set_permissions(&object, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(&object, b"corrupt shared object").unwrap();
    fs::set_permissions(&object, fs::Permissions::from_mode(0o400)).unwrap();
    fs::write(rig.dir.path().join("notes/Folder/Document.md"), external).unwrap();
    let candidate = rig
        .workspace
        .prepare_external_file(rig.initial.node_id)
        .await
        .unwrap()
        .observe()
        .unwrap();
    move_parent(&mut rig, folder).await;
    let head = rig.workspace.state.head_seq().await.unwrap();
    assert_eq!(
        rig.workspace.commit_external_file(candidate).await,
        Err(Error::InvalidFormat)
    );
    assert!(!rig.workspace.ready());
    assert_eq!(rig.workspace.state.head_seq().await.unwrap(), head);
    assert_eq!(
        fs::read(rig.dir.path().join("notes/Moved/Document.md")).unwrap(),
        external
    );
    assert_eq!(fs::read(object).unwrap(), b"corrupt shared object");
}

#[tokio::test]
async fn unreferenced_proposed_corruption_in_stale_candidate_does_not_block_workspace() {
    let mut rig = Rig::new().await;
    let external = b"unreferenced external content\n";
    let folder = nest_document(&mut rig).await;
    let object = rig
        .dir
        .path()
        .join("state/objects/sha256")
        .join(digest(external));
    fs::write(&object, b"corrupt unreferenced object").unwrap();
    fs::write(rig.dir.path().join("notes/Folder/Document.md"), external).unwrap();
    let candidate = rig
        .workspace
        .prepare_external_file(rig.initial.node_id)
        .await
        .unwrap()
        .observe()
        .unwrap();
    move_parent(&mut rig, folder).await;
    let head = rig.workspace.state.head_seq().await.unwrap();
    assert_eq!(
        rig.workspace.commit_external_file(candidate).await,
        Err(Error::ExternalChange)
    );
    assert!(rig.workspace.ready());
    assert_eq!(rig.workspace.state.head_seq().await.unwrap(), head);
    assert_eq!(
        fs::read(rig.dir.path().join("notes/Moved/Document.md")).unwrap(),
        external
    );
    assert_eq!(fs::read(object).unwrap(), b"corrupt unreferenced object");
}

#[tokio::test]
async fn candidate_retries_when_an_api_publishes_the_same_object_first() {
    let mut rig = Rig::new().await;
    let external = b"shared bytes from different sources\n";
    fs::write(rig.dir.path().join("notes/Other.md"), b"other before\n").unwrap();
    let other = rig.workspace.adopt_root_document("Other.md").await.unwrap();
    fs::write(rig.dir.path().join("notes/Document.md"), external).unwrap();
    let candidate = rig
        .workspace
        .prepare_external_file(rig.initial.node_id)
        .await
        .unwrap()
        .observe()
        .unwrap();
    let request = SaveContent {
        epoch: rig.workspace.epoch(),
        stream_id: rig.stream,
        operation_seq: 1,
        operation_id: op_id(),
        node_id: other.node_id,
        expected_content_rev: other.content_rev,
        content_hash: digest(external),
        bytes: external.to_vec(),
    };
    assert!(matches!(
        rig.workspace.save(request).await.unwrap().outcome,
        Outcome::Saved(_)
    ));
    let head = rig.workspace.state.head_seq().await.unwrap();
    assert_eq!(
        rig.workspace.commit_external_file(candidate).await,
        Err(Error::ExternalChange)
    );
    assert!(rig.workspace.ready());
    assert_eq!(rig.workspace.state.head_seq().await.unwrap(), head);
    let retry = rig
        .workspace
        .prepare_external_file(rig.initial.node_id)
        .await
        .unwrap()
        .observe()
        .unwrap();
    let published = rig
        .workspace
        .commit_external_file(retry)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(published.content_hash, digest(external));
    assert_eq!(
        rig.workspace.read(published.node_id).await.unwrap().1,
        external
    );
    assert_eq!(rig.workspace.read(other.node_id).await.unwrap().1, external);
}

#[tokio::test]
async fn recovery_only_object_corruption_is_not_hidden_by_ancestor_move() {
    use notryx_protocol::NodeKind;
    use std::os::unix::fs::PermissionsExt;
    let (mut rig, original_request) = super::recovery::conflict(Some(b"conflict state\n")).await;
    rig.workspace
        .resolve_recovery(original_request.operation_id)
        .await
        .unwrap();
    assert!(rig.workspace.ready());
    let hash = &original_request.content_hash;
    let current_refs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM nodes WHERE content_hash=?")
        .bind(hash)
        .fetch_one(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    let recovery_refs: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM recovery_objects WHERE hash=?")
            .bind(hash)
            .fetch_one(&mut rig.workspace.state.connection)
            .await
            .unwrap();
    assert_eq!(current_refs, 0);
    assert!(recovery_refs > 0);
    let root = rig.workspace.state.root_id;
    let folder = CreateNode {
        epoch: rig.workspace.epoch(),
        stream_id: rig.stream,
        operation_seq: 2,
        operation_id: op_id(),
        node_id: node_id(),
        parent_id: root,
        name: "Folder".into(),
        kind: NodeKind::Folder,
        upload_id: None,
    };
    let Outcome::Created(folder) = rig.workspace.create_node(folder).await.unwrap().outcome else {
        panic!("expected folder");
    };
    let node = rig
        .workspace
        .state
        .node(rig.initial.node_id)
        .await
        .unwrap()
        .unwrap();
    let moved = MoveNode {
        epoch: rig.workspace.epoch(),
        stream_id: rig.stream,
        operation_seq: 3,
        operation_id: op_id(),
        node_id: node.node_id,
        expected_meta_rev: node.meta_rev,
        expected_parent_id: root,
        parent_id: folder.node_id,
        name: "Document.md".into(),
    };
    assert!(matches!(
        rig.workspace.move_node(moved).await.unwrap().outcome,
        Outcome::Moved(_)
    ));
    let object = rig.dir.path().join("state/objects/sha256").join(hash);
    fs::set_permissions(&object, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(&object, b"corrupt recovery object").unwrap();
    fs::set_permissions(&object, fs::Permissions::from_mode(0o400)).unwrap();
    fs::write(
        rig.dir.path().join("notes/Folder/Document.md"),
        &original_request.bytes,
    )
    .unwrap();
    let candidate = rig
        .workspace
        .prepare_external_file(rig.initial.node_id)
        .await
        .unwrap()
        .observe()
        .unwrap();
    let moved = MoveNode {
        epoch: rig.workspace.epoch(),
        stream_id: rig.stream,
        operation_seq: 4,
        operation_id: op_id(),
        node_id: folder.node_id,
        expected_meta_rev: folder.meta_rev,
        expected_parent_id: root,
        parent_id: root,
        name: "Moved".into(),
    };
    assert!(matches!(
        rig.workspace.move_node(moved).await.unwrap().outcome,
        Outcome::Moved(_)
    ));
    let head = rig.workspace.state.head_seq().await.unwrap();
    assert_eq!(
        rig.workspace.commit_external_file(candidate).await,
        Err(Error::InvalidFormat)
    );
    assert!(!rig.workspace.ready());
    assert_eq!(rig.workspace.state.head_seq().await.unwrap(), head);
    assert_eq!(
        fs::read(rig.dir.path().join("notes/Moved/Document.md")).unwrap(),
        original_request.bytes
    );
    assert_eq!(fs::read(object).unwrap(), b"corrupt recovery object");
}

#[tokio::test]
async fn reference_query_failure_disables_writes_and_preserves_files() {
    use std::os::unix::fs::PermissionsExt;
    let mut rig = Rig::new().await;
    let object = rig
        .dir
        .path()
        .join("state/objects/sha256")
        .join(&rig.initial.content_hash);
    fs::set_permissions(&object, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(&object, b"corrupt current object").unwrap();
    fs::set_permissions(&object, fs::Permissions::from_mode(0o400)).unwrap();
    let path = rig.dir.path().join("notes/Document.md");
    fs::write(&path, b"external content\n").unwrap();
    let candidate = rig
        .workspace
        .prepare_external_file(rig.initial.node_id)
        .await
        .unwrap()
        .observe()
        .unwrap();
    sqlx::query("DROP TABLE recovery_objects")
        .execute(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    assert_eq!(
        rig.workspace.commit_external_file(candidate).await,
        Err(Error::Storage)
    );
    assert!(!rig.workspace.ready());
    assert_eq!(
        rig.workspace.state.head_seq().await.unwrap(),
        rig.initial.last_change_seq
    );
    assert_eq!(fs::read(path).unwrap(), b"external content\n");
    assert_eq!(fs::read(object).unwrap(), b"corrupt current object");
}

#[tokio::test]
async fn candidates_are_rejected_after_api_save_and_ancestor_move() {
    use notryx_protocol::NodeKind;
    let mut rig = Rig::new().await;
    let candidate = rig
        .workspace
        .prepare_external_file(rig.initial.node_id)
        .await
        .unwrap()
        .observe()
        .unwrap();
    let Outcome::Saved(saved) = rig
        .workspace
        .save(rig.command(b"api save\n"))
        .await
        .unwrap()
        .outcome
    else {
        panic!("expected saved document");
    };
    assert_eq!(
        rig.workspace.commit_external_file(candidate).await,
        Err(Error::ExternalChange)
    );
    assert_eq!(
        rig.workspace.read(saved.node_id).await.unwrap().1,
        b"api save\n"
    );

    let root = rig.workspace.state.root_id;
    let create = CreateNode {
        epoch: rig.workspace.epoch(),
        stream_id: rig.stream,
        operation_seq: 2,
        operation_id: op_id(),
        node_id: node_id(),
        parent_id: root,
        name: "Folder".into(),
        kind: NodeKind::Folder,
        upload_id: None,
    };
    let Outcome::Created(folder) = rig.workspace.create_node(create).await.unwrap().outcome else {
        panic!("expected folder");
    };
    let moving = NodeSnapshot::from(&saved);
    let command = MoveNode {
        epoch: rig.workspace.epoch(),
        stream_id: rig.stream,
        operation_seq: 3,
        operation_id: op_id(),
        node_id: moving.node_id,
        expected_meta_rev: moving.meta_rev.clone(),
        expected_parent_id: root,
        parent_id: folder.node_id,
        name: "Document.md".into(),
    };
    assert!(matches!(
        rig.workspace.move_node(command).await.unwrap().outcome,
        Outcome::Moved(_)
    ));
    let candidate = rig
        .workspace
        .prepare_external_file(saved.node_id)
        .await
        .unwrap()
        .observe()
        .unwrap();
    let command = MoveNode {
        epoch: rig.workspace.epoch(),
        stream_id: rig.stream,
        operation_seq: 4,
        operation_id: op_id(),
        node_id: folder.node_id,
        expected_meta_rev: folder.meta_rev.clone(),
        expected_parent_id: root,
        parent_id: root,
        name: "Moved".into(),
    };
    assert!(matches!(
        rig.workspace.move_node(command).await.unwrap().outcome,
        Outcome::Moved(_)
    ));
    let head = rig.workspace.state.head_seq().await.unwrap();
    assert_eq!(
        rig.workspace.commit_external_file(candidate).await,
        Err(Error::ExternalChange)
    );
    assert_eq!(rig.workspace.state.head_seq().await.unwrap(), head);
    assert_eq!(
        fs::read(rig.dir.path().join("notes/Moved/Document.md")).unwrap(),
        b"api save\n"
    );
}

#[tokio::test]
async fn sync_failures_at_object_file_and_parent_keep_existing_state() {
    let mut rig = Rig::new().await;
    let path = rig.dir.path().join("notes/Document.md");
    for (index, failed_stage) in [
        StorageStage::BeforeObjectFileSync,
        StorageStage::BeforeObjectDirectorySync,
        StorageStage::BeforeRecoveryFileSync,
        StorageStage::BeforeRecoveryDirectorySync,
    ]
    .into_iter()
    .enumerate()
    {
        let external = format!("external content {index}\n");
        fs::write(&path, &external).unwrap();
        rig.workspace.hook = None;
        let candidate = rig
            .workspace
            .prepare_external_file(rig.initial.node_id)
            .await
            .unwrap()
            .observe_with_storage_hook(move |stage| {
                if stage == failed_stage {
                    return Err(Error::Storage);
                }
                Ok(())
            })
            .unwrap();
        rig.workspace.hook = Some(Box::new(move |phase| {
            if phase == Phase::Storage(failed_stage) {
                return Err(Error::Storage);
            }
            Ok(())
        }));
        assert_eq!(
            rig.workspace.commit_external_file(candidate).await,
            Err(Error::Storage),
            "{failed_stage:?}"
        );
        assert_eq!(
            rig.workspace.state.head_seq().await.unwrap(),
            rig.initial.last_change_seq
        );
        assert_eq!(
            rig.workspace.read(rig.initial.node_id).await.unwrap().0,
            rig.initial
        );
        assert_eq!(fs::read(&path).unwrap(), external.as_bytes());
        assert!(rig.workspace.ready());
    }
}

#[tokio::test]
async fn atomic_replacement_between_observations_rejects_unstable_candidate() {
    let mut rig = Rig::new().await;
    let path = rig.dir.path().join("notes/Document.md");
    fs::write(&path, b"first state\n").unwrap();
    let replacement = rig.dir.path().join("replacement");
    fs::write(&replacement, b"new atomic state\n").unwrap();
    let probe = rig
        .workspace
        .prepare_external_file(rig.initial.node_id)
        .await
        .unwrap();
    let moved_path = path.clone();
    assert!(matches!(
        probe.observe_with_hook(move || fs::rename(replacement, moved_path).unwrap()),
        Err(Error::ExternalChange)
    ));
    assert_eq!(
        rig.workspace.state.head_seq().await.unwrap(),
        rig.initial.last_change_seq
    );
    assert_eq!(fs::read(path).unwrap(), b"new atomic state\n");
}

#[tokio::test]
async fn same_hash_observation_does_not_advance_head() {
    let mut rig = Rig::new().await;
    let path = rig.dir.path().join("notes/Document.md");
    let original = fs::read(&path).unwrap();
    let candidate = rig
        .workspace
        .prepare_external_file(rig.initial.node_id)
        .await
        .unwrap()
        .observe()
        .unwrap();
    assert_eq!(
        rig.workspace.commit_external_file(candidate).await.unwrap(),
        None
    );
    assert_eq!(
        rig.workspace.state.head_seq().await.unwrap(),
        rig.initial.last_change_seq
    );
    assert_eq!(fs::read(path).unwrap(), original);
}

#[tokio::test]
async fn observation_rejects_unreadable_and_escaping_paths_without_delete() {
    use notryx_server_storage::StorageError;
    use std::os::unix::fs::PermissionsExt;
    let mut rig = Rig::new().await;
    let outside = rig.dir.path().join("outside.md");
    fs::write(&outside, b"outside source\n").unwrap();
    let reader = rig.workspace.files.observation_reader().unwrap();
    assert!(matches!(
        reader.observe("../outside.md"),
        Err(StorageError::InvalidName)
    ));
    let path = rig.dir.path().join("notes/Document.md");
    let original = fs::read(&path).unwrap();
    let probe = rig
        .workspace
        .prepare_external_file(rig.initial.node_id)
        .await
        .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
    let result = probe.observe();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(matches!(result, Err(Error::Storage)));
    assert_eq!(
        rig.workspace.state.head_seq().await.unwrap(),
        rig.initial.last_change_seq
    );
    assert!(
        !rig.workspace
            .state
            .node(rig.initial.node_id)
            .await
            .unwrap()
            .unwrap()
            .deleted
    );
    assert_eq!(fs::read(path).unwrap(), original);
    assert_eq!(fs::read(outside).unwrap(), b"outside source\n");
}

#[tokio::test]
async fn external_preparation_runs_fsync_without_holding_coordinator() {
    use std::sync::mpsc;
    use std::time::Duration;
    let mut rig = Rig::new().await;
    let path = rig.dir.path().join("notes/Document.md");
    let other_path = rig.dir.path().join("notes/Other.md");
    fs::write(&other_path, b"other original\n").unwrap();
    let other = rig.workspace.adopt_root_document("Other.md").await.unwrap();
    fs::write(&path, b"external candidate\n").unwrap();
    let probe = rig
        .workspace
        .prepare_external_file(rig.initial.node_id)
        .await
        .unwrap();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        probe.observe_with_storage_hook(move |stage| {
            if stage == StorageStage::BeforeRecoveryFileSync {
                entered_tx.send(()).map_err(|_| Error::Storage)?;
                release_rx
                    .recv_timeout(Duration::from_secs(15))
                    .map_err(|_| Error::Storage)?;
            }
            Ok(())
        })
    });
    let entered = entered_rx.recv_timeout(Duration::from_secs(5));
    if entered.is_err() {
        let _ = release_tx.send(());
        let _ = worker.join();
        panic!("external preparation did not reach worker fsync");
    }
    let request = SaveContent {
        epoch: rig.workspace.epoch(),
        stream_id: rig.stream,
        operation_seq: 1,
        operation_id: op_id(),
        node_id: other.node_id,
        expected_content_rev: other.content_rev.clone(),
        content_hash: digest(b"api content\n"),
        bytes: b"api content\n".to_vec(),
    };
    let receipt = rig.workspace.save(request).await.unwrap();
    assert!(matches!(receipt.outcome, Outcome::Saved(_)));
    release_tx.send(()).unwrap();
    let candidate = worker.join().unwrap().unwrap();
    assert!(
        rig.workspace
            .commit_external_file(candidate)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(fs::read(other_path).unwrap(), b"api content\n");
    assert_eq!(fs::read(path).unwrap(), b"external candidate\n");
}

struct RuntimeRig {
    dir: tempfile::TempDir,
    runtime: crate::runtime::WorkspaceRuntime,
    document: Document,
    other: Document,
    paired: notryx_protocol::PairResponse,
}

impl RuntimeRig {
    async fn new() -> Self {
        Self::new_with_watch(false).await
    }

    async fn new_with_watch(watch_enabled: bool) -> Self {
        use crate::config::WorkspaceConfig;
        use crate::runtime::WorkspaceRuntime;
        use notryx_protocol::{AdminRequest, AdminResult, PairRequest};
        let dir = tempfile::tempdir().unwrap();
        let notes = dir.path().join("notes");
        let state = dir.path().join("state");
        fs::create_dir(&notes).unwrap();
        fs::create_dir(&state).unwrap();
        let mut workspace = Workspace::initialize(&notes, &state).await.unwrap();
        fs::write(notes.join("Document.md"), b"original document\n").unwrap();
        fs::write(notes.join("Other.md"), b"other original\n").unwrap();
        let document = workspace.adopt_root_document("Document.md").await.unwrap();
        let other = workspace.adopt_root_document("Other.md").await.unwrap();
        workspace.close().await.unwrap();
        let config = WorkspaceConfig {
            notes_root: notes,
            state_root: state,
            uploads: Default::default(),
        };
        let mut runtime = if watch_enabled {
            WorkspaceRuntime::start(config).await
        } else {
            WorkspaceRuntime::test_start_without_known_file_watch(config).await
        }
        .unwrap();
        runtime.test_wait_startup_scan().await;
        assert!(!runtime.handle().known_file_scan_incomplete());
        let handle = runtime.handle();
        let AdminResult::Status {
            workspace_id,
            owner_principal_id,
            ..
        } = handle.admin(AdminRequest::Status {}).await.unwrap()
        else {
            panic!("status")
        };
        let AdminResult::Pairing { pairing_secret, .. } = handle
            .admin(AdminRequest::IssuePairing {
                principal_id: owner_principal_id,
            })
            .await
            .unwrap()
        else {
            panic!("pairing")
        };
        let paired = handle
            .pair(PairRequest {
                protocol: notryx_protocol::CONTRACT.into(),
                workspace_id,
                pairing_secret,
            })
            .await
            .unwrap();
        Self {
            dir,
            runtime,
            document,
            other,
            paired,
        }
    }

    async fn new_with_registered_folder_watch() -> (Self, NodeId) {
        use crate::config::WorkspaceConfig;
        use crate::runtime::WorkspaceRuntime;
        let Self {
            dir,
            runtime,
            document,
            other,
            paired,
        } = Self::new().await;
        runtime.shutdown().await.unwrap();
        let notes = dir.path().join("notes");
        let state = dir.path().join("state");
        let mut workspace = Workspace::open(&notes, &state).await.unwrap();
        let root = workspace.state.root_id;
        let stream = workspace.register_stream().await.unwrap();
        let folder = node_id();
        assert!(matches!(
            workspace
                .create_node(CreateNode {
                    epoch: workspace.epoch(),
                    stream_id: stream,
                    operation_seq: 1,
                    operation_id: op_id(),
                    node_id: folder,
                    parent_id: root,
                    name: "Folder".into(),
                    kind: notryx_protocol::NodeKind::Folder,
                    upload_id: None,
                })
                .await
                .unwrap()
                .outcome,
            Outcome::Created(_)
        ));
        workspace.close().await.unwrap();
        let mut runtime = WorkspaceRuntime::start(WorkspaceConfig {
            notes_root: notes,
            state_root: state,
            uploads: Default::default(),
        })
        .await
        .unwrap();
        runtime.test_wait_startup_scan().await;
        (
            Self {
                dir,
                runtime,
                document,
                other,
                paired,
            },
            folder,
        )
    }
}

#[tokio::test]
async fn startup_scan_ingests_existing_external_change_without_manual_reconcile() {
    use crate::config::WorkspaceConfig;
    use crate::runtime::WorkspaceRuntime;
    use std::time::Duration;

    let RuntimeRig {
        dir,
        runtime,
        document,
        other,
        paired,
    } = RuntimeRig::new().await;
    let handle = runtime.handle();
    let prior = SaveContent {
        epoch: paired.workspace_epoch,
        stream_id: paired.device_stream_id.parse().unwrap(),
        operation_seq: 1,
        operation_id: op_id(),
        node_id: other.node_id,
        expected_content_rev: other.content_rev.clone(),
        content_hash: digest(b"earlier api edit\n"),
        bytes: b"earlier api edit\n".to_vec(),
    };
    let receipt = handle
        .save_content(paired.device_token.clone(), prior.clone())
        .await
        .unwrap();
    let initial = handle
        .server_info(paired.device_token.clone())
        .await
        .unwrap();
    runtime.shutdown().await.unwrap();

    let notes = dir.path().join("notes");
    let state = dir.path().join("state");
    let source = notes.join("Document.md");
    let external = b"\xef\xbb\xbf# changed while stopped\r\n";
    fs::write(&source, external).unwrap();
    let runtime = WorkspaceRuntime::start(WorkspaceConfig {
        notes_root: notes,
        state_root: state.clone(),
        uploads: Default::default(),
    })
    .await
    .unwrap();
    let handle = runtime.handle();
    let head = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let current = handle
                .server_info(paired.device_token.clone())
                .await
                .unwrap()
                .head_seq;
            if current != initial.head_seq {
                break current;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    let receipt_after = handle
        .operation(paired.device_token.clone(), prior.operation_id)
        .await
        .unwrap();
    let source_bytes = fs::read(source).unwrap();
    let old_object = fs::read(state.join("objects/sha256").join(&document.content_hash)).unwrap();
    let result = if let Ok(head) = head {
        let (changed, bytes) = handle
            .content(paired.device_token.clone(), document.node_id, None)
            .await
            .unwrap();
        let page = handle
            .changes(paired.device_token.clone(), initial.head_seq, Some(head))
            .await
            .unwrap();
        Some((head, changed, bytes, page))
    } else {
        None
    };
    runtime.shutdown().await.unwrap();

    let (head, changed, bytes, page) =
        result.expect("startup scan did not ingest the existing external change");
    assert_eq!(head.get(), initial.head_seq.get() + 1);
    assert_eq!(changed.node_id, document.node_id);
    assert_eq!(changed.meta_rev, document.meta_rev);
    assert_ne!(changed.content_rev, document.content_rev);
    assert_eq!(changed.content_hash, digest(external));
    assert_eq!(bytes, external);
    assert_eq!(page.covered_through, head);
    assert_eq!(page.nodes.len(), 1);
    assert_eq!(page.nodes[0].node_id, document.node_id);
    assert_eq!(receipt_after, receipt);
    assert_eq!(source_bytes, external);
    assert_eq!(old_object, b"original document\n");
}

#[tokio::test]
async fn running_server_detects_existing_file_external_write_after_watch_registration() {
    use std::time::Duration;

    let rig = RuntimeRig::new_with_watch(true).await;
    let handle = rig.runtime.handle();
    let mut prior_notices = handle.event_subscription();
    let prior = SaveContent {
        epoch: rig.paired.workspace_epoch,
        stream_id: rig.paired.device_stream_id.parse().unwrap(),
        operation_seq: 1,
        operation_id: op_id(),
        node_id: rig.other.node_id,
        expected_content_rev: rig.other.content_rev.clone(),
        content_hash: digest(b"earlier api edit\n"),
        bytes: b"earlier api edit\n".to_vec(),
    };
    let receipt = handle
        .save_content(rig.paired.device_token.clone(), prior.clone())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), prior_notices.recv())
        .await
        .unwrap()
        .unwrap();
    drop(prior_notices);
    let initial = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap();
    let source = rig.dir.path().join("notes/Document.md");
    let original_object = rig
        .dir
        .path()
        .join("state/objects/sha256")
        .join(&rig.document.content_hash);
    let external = b"\xef\xbb\xbf# external while running\r\n";
    let mut notices = handle.event_subscription();
    fs::write(&source, external).unwrap();
    let changed_head = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let current = handle
                .server_info(rig.paired.device_token.clone())
                .await
                .unwrap()
                .head_seq;
            if current != initial.head_seq {
                break current;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    let receipt_after = handle
        .operation(rig.paired.device_token.clone(), prior.operation_id)
        .await
        .unwrap();
    let source_bytes = fs::read(source).unwrap();
    let old_object = fs::read(original_object).unwrap();
    let result = if let Ok(head) = changed_head {
        let (document, bytes) = handle
            .content(rig.paired.device_token.clone(), rig.document.node_id, None)
            .await
            .unwrap();
        let page = handle
            .changes(
                rig.paired.device_token.clone(),
                initial.head_seq,
                Some(head),
            )
            .await
            .unwrap();
        let notice = tokio::time::timeout(Duration::from_secs(2), notices.recv())
            .await
            .is_ok_and(|received| received.is_ok());
        tokio::time::sleep(Duration::from_millis(400)).await;
        let settled_head = handle
            .server_info(rig.paired.device_token.clone())
            .await
            .unwrap()
            .head_seq;
        Some((head, document, bytes, page, notice, settled_head))
    } else {
        None
    };
    rig.runtime.shutdown().await.unwrap();

    let (head, document, bytes, page, notice, settled_head) =
        result.expect("running server did not detect the external write");
    assert_eq!(head.get(), initial.head_seq.get() + 1);
    assert_eq!(settled_head, head);
    assert_eq!(document.node_id, rig.document.node_id);
    assert_eq!(document.meta_rev, rig.document.meta_rev);
    assert_ne!(document.content_rev, rig.document.content_rev);
    assert_eq!(document.content_hash, digest(external));
    assert_eq!(bytes, external);
    assert_eq!(page.covered_through, head);
    assert_eq!(page.nodes.len(), 1);
    assert_eq!(page.nodes[0].node_id, rig.document.node_id);
    assert!(notice);
    assert_eq!(receipt_after, receipt);
    assert_eq!(source_bytes, external);
    assert_eq!(old_object, b"original document\n");
}

#[tokio::test]
async fn watcher_discovers_new_markdown_in_registered_folder_once() {
    use crate::config::WorkspaceConfig;
    use crate::runtime::WorkspaceRuntime;
    use std::time::Duration;

    let RuntimeRig {
        dir,
        runtime,
        document,
        other,
        paired,
        ..
    } = RuntimeRig::new().await;
    let handle = runtime.handle();
    let prior = SaveContent {
        epoch: paired.workspace_epoch,
        stream_id: paired.device_stream_id.parse().unwrap(),
        operation_seq: 1,
        operation_id: op_id(),
        node_id: other.node_id,
        expected_content_rev: other.content_rev,
        content_hash: digest(b"prior receipt\n"),
        bytes: b"prior receipt\n".to_vec(),
    };
    let receipt = handle
        .save_content(paired.device_token.clone(), prior.clone())
        .await
        .unwrap();
    runtime.shutdown().await.unwrap();
    let notes = dir.path().join("notes");
    let state = dir.path().join("state");
    let mut workspace = Workspace::open(&notes, &state).await.unwrap();
    let root = workspace.state.root_id;
    let stream = workspace.register_stream().await.unwrap();
    let folder_id = node_id();
    assert!(matches!(
        workspace
            .create_node(CreateNode {
                epoch: workspace.epoch(),
                stream_id: stream,
                operation_seq: 1,
                operation_id: op_id(),
                node_id: folder_id,
                parent_id: root,
                name: "Folder".into(),
                kind: notryx_protocol::NodeKind::Folder,
                upload_id: None,
            })
            .await
            .unwrap()
            .outcome,
        Outcome::Created(_)
    ));
    workspace.close().await.unwrap();
    let mut runtime = WorkspaceRuntime::start(WorkspaceConfig {
        notes_root: notes.clone(),
        state_root: state.clone(),
        uploads: Default::default(),
    })
    .await
    .unwrap();
    runtime.test_wait_startup_scan().await;
    let handle = runtime.handle();
    let initial = handle
        .server_info(paired.device_token.clone())
        .await
        .unwrap();
    let parent_before = handle
        .node_state(paired.device_token.clone(), folder_id)
        .await
        .unwrap();
    let mut notices = handle.event_subscription();
    let external = b"\xef\xbb\xbf# New external document\r\n\r\n<!-- preserved -->\r\n";
    let source = notes.join("Folder/New.md");
    fs::write(&source, external).unwrap();
    let observed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let head = handle
                .server_info(paired.device_token.clone())
                .await
                .unwrap()
                .head_seq;
            if head != initial.head_seq {
                break head;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    let outcome = if let Ok(head) = observed {
        let page = handle
            .changes(paired.device_token.clone(), initial.head_seq, Some(head))
            .await
            .unwrap();
        let created = page
            .nodes
            .iter()
            .find(|node| node.parent_id == Some(folder_id) && node.name == "New.md")
            .cloned();
        let parent = handle
            .node_state(paired.device_token.clone(), folder_id)
            .await
            .unwrap();
        let bytes = match &created {
            Some(node) => Some(
                handle
                    .content(paired.device_token.clone(), node.node_id, None)
                    .await
                    .unwrap()
                    .1,
            ),
            None => None,
        };
        let notice = tokio::time::timeout(Duration::from_secs(2), notices.recv())
            .await
            .is_ok_and(|value| value.is_ok());
        tokio::time::sleep(Duration::from_millis(400)).await;
        let settled = handle
            .server_info(paired.device_token.clone())
            .await
            .unwrap()
            .head_seq;
        Some((head, page, created, parent, bytes, notice, settled))
    } else {
        None
    };
    let preserved_receipt = handle
        .operation(paired.device_token.clone(), prior.operation_id)
        .await
        .unwrap();
    let original = fs::read(notes.join("Document.md")).unwrap();
    let original_object =
        fs::read(state.join("objects/sha256").join(&document.content_hash)).unwrap();
    runtime.shutdown().await.unwrap();
    let (head, page, created, parent, bytes, notice, settled) =
        outcome.expect("watcher did not discover new markdown");
    let created = created.expect("journal did not include the new node");
    assert_eq!(head.get(), initial.head_seq.get() + 1);
    assert_eq!(settled, head);
    assert_eq!(page.covered_through, head);
    assert_eq!(page.nodes.len(), 2);
    assert!(page.nodes.iter().any(|node| node.node_id == folder_id));
    assert_eq!(created.kind, notryx_protocol::NodeKind::File);
    assert_eq!(
        created.content_hash.as_deref(),
        Some(format!("sha256:{}", digest(external)).as_str())
    );
    assert_eq!(bytes.unwrap(), external);
    assert_eq!(
        parent.children_generation.get(),
        parent_before.children_generation.get() + 1
    );
    assert_eq!(parent.meta_rev, parent_before.meta_rev);
    assert_eq!(parent.last_change_seq, head);
    assert!(notice);
    assert_eq!(preserved_receipt, receipt);
    assert_eq!(fs::read(source).unwrap(), external);
    assert_eq!(original, b"original document\n");
    assert_eq!(original_object, b"original document\n");
}

#[tokio::test]
async fn startup_scan_discovers_markdown_created_while_server_stopped() {
    check_registered_folder_scan_discovers_new_markdown(true).await;
}

#[tokio::test]
async fn overflow_scan_discovers_markdown_after_suppressed_event() {
    check_registered_folder_scan_discovers_new_markdown(false).await;
}

#[tokio::test]
async fn startup_new_file_scan_skips_registered_folder_named_markdown() {
    use crate::config::WorkspaceConfig;
    use crate::runtime::WorkspaceRuntime;
    let (rig, folder) = RuntimeRig::new_with_registered_folder_watch().await;
    let RuntimeRig {
        dir,
        runtime,
        paired,
        ..
    } = rig;
    runtime.shutdown().await.unwrap();
    let notes = dir.path().join("notes");
    let state = dir.path().join("state");
    let mut workspace = Workspace::open(&notes, &state).await.unwrap();
    let stream = workspace.register_stream().await.unwrap();
    let nested_folder = node_id();
    assert!(matches!(
        workspace
            .create_node(CreateNode {
                epoch: workspace.epoch(),
                stream_id: stream,
                operation_seq: 1,
                operation_id: op_id(),
                node_id: nested_folder,
                parent_id: folder,
                name: "Notes.md".into(),
                kind: notryx_protocol::NodeKind::Folder,
                upload_id: None,
            })
            .await
            .unwrap()
            .outcome,
        Outcome::Created(_)
    ));
    let before = workspace.state.head_seq().await.unwrap();
    workspace.close().await.unwrap();
    let source = notes.join("Folder/New.md");
    fs::write(&source, b"new document\n").unwrap();
    let mut runtime = WorkspaceRuntime::start(WorkspaceConfig {
        notes_root: notes.clone(),
        state_root: state,
        uploads: Default::default(),
    })
    .await
    .unwrap();
    runtime.test_wait_startup_scan().await;
    let handle = runtime.handle();
    let head = handle
        .server_info(paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let node = handle
        .node_state(paired.device_token.clone(), nested_folder)
        .await
        .unwrap();
    let page = handle
        .changes(paired.device_token.clone(), before, Some(head))
        .await
        .unwrap();
    let incomplete = handle.known_file_scan_incomplete();
    runtime.shutdown().await.unwrap();
    assert_eq!(head.get(), before.get() + 1);
    assert_eq!(node.kind, notryx_protocol::NodeKind::Folder);
    assert!(
        page.nodes
            .iter()
            .any(|node| node.name == "New.md" && node.parent_id == Some(folder))
    );
    assert!(
        !incomplete,
        "a registered folder named .md must not fail the file scan"
    );
    assert!(notes.join("Folder/Notes.md").is_dir());
    assert_eq!(fs::read(source).unwrap(), b"new document\n");
}

#[tokio::test]
async fn startup_new_file_scan_continues_after_non_utf8_name() {
    use std::os::unix::ffi::OsStrExt;
    check_startup_new_file_scan_continues_after_invalid_name(std::ffi::OsStr::from_bytes(
        b"\xff.md",
    ))
    .await;
}

#[tokio::test]
async fn startup_new_file_scan_continues_after_disallowed_name() {
    check_startup_new_file_scan_continues_after_invalid_name(std::ffi::OsStr::new("bad\\name.md"))
        .await;
}

#[tokio::test]
async fn watcher_external_file_deletion_waits_then_preserves_history() {
    check_external_file_deletion(0).await;
}

#[tokio::test]
async fn watcher_discovers_external_markdown_in_api_created_folder() {
    use std::time::Duration;
    let rig = RuntimeRig::new_with_watch(true).await;
    let handle = rig.runtime.handle();
    let folder_id = node_id();
    let request = CreateNode {
        epoch: rig.paired.workspace_epoch,
        stream_id: rig.paired.device_stream_id.parse().unwrap(),
        operation_seq: 1,
        operation_id: op_id(),
        node_id: folder_id,
        parent_id: rig.document.parent_id,
        name: "Created folder".into(),
        kind: notryx_protocol::NodeKind::Folder,
        upload_id: None,
    };
    let receipt = handle
        .create_node(rig.paired.device_token.clone(), request.clone())
        .await
        .unwrap();
    assert!(matches!(&receipt.outcome, Outcome::Created(_)));
    let before = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let file = rig.dir.path().join("notes/Created folder/External.md");
    fs::write(&file, b"external document\n").unwrap();
    let found = tokio::time::timeout(Duration::from_secs(6), async {
        loop {
            let head = handle
                .server_info(rig.paired.device_token.clone())
                .await
                .unwrap()
                .head_seq;
            if head.get() > before.get() {
                let changes = handle
                    .changes(rig.paired.device_token.clone(), before, Some(head))
                    .await
                    .unwrap();
                if changes
                    .nodes
                    .iter()
                    .any(|node| node.parent_id == Some(folder_id) && node.name == "External.md")
                {
                    break head;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    let after = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let receipt_after = handle
        .operation(rig.paired.device_token.clone(), request.operation_id)
        .await
        .unwrap();
    let bytes = fs::read(&file).unwrap();
    let original = fs::read(
        rig.dir
            .path()
            .join("state/objects/sha256")
            .join(&rig.document.content_hash),
    )
    .unwrap();
    rig.runtime.shutdown().await.unwrap();
    assert!(
        found.is_ok(),
        "watcher did not discover Markdown in the API-created folder"
    );
    assert_eq!(after.get(), before.get() + 1);
    assert_eq!(receipt_after, receipt);
    assert_eq!(bytes, b"external document\n");
    assert_eq!(original, b"original document\n");
}

#[tokio::test]
async fn watcher_discovers_external_folder_then_existing_and_later_markdown() {
    use std::time::Duration;

    let rig = RuntimeRig::new_with_watch(true).await;
    let handle = rig.runtime.handle();
    let prior = SaveContent {
        epoch: rig.paired.workspace_epoch,
        stream_id: rig.paired.device_stream_id.parse().unwrap(),
        operation_seq: 1,
        operation_id: op_id(),
        node_id: rig.other.node_id,
        expected_content_rev: rig.other.content_rev.clone(),
        content_hash: digest(b"prior receipt\n"),
        bytes: b"prior receipt\n".to_vec(),
    };
    let receipt = handle
        .save_content(rig.paired.device_token.clone(), prior.clone())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let before = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let folder_path = rig.dir.path().join("notes/External folder");
    fs::create_dir(&folder_path).unwrap();
    fs::write(folder_path.join("Existing.md"), b"existing content\n").unwrap();

    let initial = tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let head = handle
                .server_info(rig.paired.device_token.clone())
                .await
                .unwrap()
                .head_seq;
            if head > before {
                let changes = handle
                    .changes(rig.paired.device_token.clone(), before, Some(head))
                    .await
                    .unwrap();
                if let Some(folder) = changes.nodes.iter().find(|node| {
                    node.parent_id == Some(rig.document.parent_id)
                        && node.name == "External folder"
                        && node.kind == notryx_protocol::NodeKind::Folder
                }) && changes.nodes.iter().any(|node| {
                    node.parent_id == Some(folder.node_id) && node.name == "Existing.md"
                }) {
                    break (folder.node_id, head);
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    let later =
        if let Some((folder_id, first_head)) = initial.as_ref().ok().copied() {
            fs::write(folder_path.join("Later.md"), b"later content\n").unwrap();
            tokio::time::timeout(Duration::from_secs(8), async {
                loop {
                    let head = handle
                        .server_info(rig.paired.device_token.clone())
                        .await
                        .unwrap()
                        .head_seq;
                    if head > first_head {
                        let changes = handle
                            .changes(rig.paired.device_token.clone(), first_head, Some(head))
                            .await
                            .unwrap();
                        if changes.nodes.iter().any(|node| {
                            node.parent_id == Some(folder_id) && node.name == "Later.md"
                        }) {
                            break head;
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .ok()
        } else {
            None
        };
    let receipt_after = handle
        .operation(rig.paired.device_token.clone(), prior.operation_id)
        .await
        .unwrap();
    let first_bytes = fs::read(folder_path.join("Existing.md")).unwrap();
    let original = fs::read(
        rig.dir
            .path()
            .join("state/objects/sha256")
            .join(&rig.document.content_hash),
    )
    .unwrap();
    rig.runtime.shutdown().await.unwrap();

    assert!(
        initial.is_ok(),
        "external folder and existing Markdown were not discovered"
    );
    assert!(
        later.is_some(),
        "the discovered folder did not watch later Markdown"
    );
    assert_eq!(initial.unwrap().1.get(), before.get() + 2);
    assert_eq!(later.unwrap().get(), before.get() + 3);
    assert_eq!(receipt_after, receipt);
    assert_eq!(first_bytes, b"existing content\n");
    assert_eq!(original, b"original document\n");
    assert_eq!(
        fs::read(folder_path.join("Later.md")).unwrap(),
        b"later content\n"
    );
}

#[tokio::test]
async fn watcher_external_folder_symlink_does_not_adopt_target() {
    use std::os::unix::fs::symlink;
    use std::time::Duration;

    let rig = RuntimeRig::new_with_watch(true).await;
    let handle = rig.runtime.handle();
    let before = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let outside = rig.dir.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("Target.md"), b"outside content\n").unwrap();
    let signal = handle.test_signal_watch_name("Linked folder");
    let link = rig.dir.path().join("notes/Linked folder");
    symlink(&outside, &link).unwrap();
    tokio::time::timeout(Duration::from_secs(5), signal)
        .await
        .unwrap()
        .unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;
    let after = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let bytes = fs::read(outside.join("Target.md")).unwrap();
    let link_still_present = fs::symlink_metadata(&link)
        .unwrap()
        .file_type()
        .is_symlink();
    rig.runtime.shutdown().await.unwrap();
    assert_eq!(after, before);
    assert!(link_still_present);
    assert_eq!(bytes, b"outside content\n");
}

#[tokio::test]
async fn watcher_external_folder_rejects_parent_replacement_after_observation() {
    use std::time::Duration;

    let (rig, folder_id) = RuntimeRig::new_with_registered_folder_watch().await;
    let handle = rig.runtime.handle();
    let before = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let (entered, release, first_done) = handle.test_trace_external_folder();
    let notes = rig.dir.path().join("notes");
    let candidate = notes.join("Folder/External folder");
    fs::create_dir(&candidate).unwrap();
    fs::write(candidate.join("Original.md"), b"original candidate\n").unwrap();
    tokio::time::timeout(Duration::from_secs(10), entered)
        .await
        .unwrap()
        .unwrap();
    let preserved = rig.dir.path().join("preserved-parent");
    fs::rename(notes.join("Folder"), &preserved).unwrap();
    fs::create_dir(notes.join("Folder")).unwrap();
    fs::create_dir(notes.join("Folder/External folder")).unwrap();
    fs::write(
        notes.join("Folder/External folder/Replacement.md"),
        b"replacement candidate\n",
    )
    .unwrap();
    release.send(()).unwrap();
    let first_result = tokio::time::timeout(Duration::from_secs(10), first_done)
        .await
        .expect("initial folder observation did not finish")
        .expect("initial folder observation result was not delivered");
    let expected_head = before.get().checked_add(2).unwrap();
    let head = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let head = handle
                .server_info(rig.paired.device_token.clone())
                .await
                .unwrap()
                .head_seq;
            if head.get() >= expected_head {
                break head;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("replacement folder was not observed again");
    let folder = handle
        .node_state(rig.paired.device_token.clone(), folder_id)
        .await
        .unwrap();
    let original = fs::read(preserved.join("External folder/Original.md")).unwrap();
    let replacement = fs::read(notes.join("Folder/External folder/Replacement.md")).unwrap();
    let page = handle
        .changes(rig.paired.device_token.clone(), before, Some(head))
        .await
        .unwrap();
    let changes: Vec<_> = page
        .nodes
        .iter()
        .map(|node| (node.last_change_seq.get(), node.name.clone(), node.deleted))
        .collect();
    let commits = handle.test_external_folder_trace();
    rig.runtime.shutdown().await.unwrap();
    assert!(
        matches!(
            &first_result,
            Err(crate::runtime::RuntimeError::Workspace(
                Error::ExternalChange
            ))
        ),
        "initial candidate result: {first_result:?}; folder commits: {commits:?}; changes: {changes:?}"
    );
    assert_eq!(
        head.get(),
        expected_head,
        "initial candidate result: {first_result:?}; folder commits: {commits:?}; changes: {changes:?}"
    );
    assert_eq!(page.covered_through, head);
    let observed_folder = page
        .nodes
        .iter()
        .find(|node| {
            node.name == "External folder"
                && node.kind == notryx_protocol::NodeKind::Folder
                && node.last_change_seq.get() == before.get() + 1
                && node.parent_id == Some(folder_id)
                && !node.deleted
        })
        .expect("replacement folder must be recorded by a fresh observation");
    let observed_file = page
        .nodes
        .iter()
        .find(|node| {
            node.name == "Replacement.md"
                && node.kind == notryx_protocol::NodeKind::File
                && node.last_change_seq.get() == expected_head
                && node.parent_id == Some(observed_folder.node_id)
                && !node.deleted
        })
        .expect("replacement file must be recorded under the newly observed folder");
    assert_eq!(
        observed_file.content_hash.as_deref(),
        Some(format!("sha256:{}", digest(b"replacement candidate\n")).as_str())
    );
    assert_eq!(
        observed_file.size.as_deref(),
        Some(b"replacement candidate\n".len().to_string().as_str())
    );
    assert_eq!(page.nodes.len(), 4);
    assert!(page.nodes.iter().any(|node| {
        node.node_id == folder_id && node.last_change_seq.get() == before.get() + 1
    }));
    assert!(page.nodes.iter().any(|node| {
        node.node_id == observed_folder.node_id && node.last_change_seq.get() == expected_head
    }));
    assert!(!page.nodes.iter().any(|node| node.name == "Original.md"));
    assert!(commits.iter().any(|(original, result)| {
        !*original
            && result.as_ref().is_ok_and(|node| {
                node.node_id == observed_folder.node_id
                    && node.last_change_seq.get() == before.get() + 1
            })
    }));
    assert!(!folder.deleted);
    assert_eq!(original, b"original candidate\n");
    assert_eq!(replacement, b"replacement candidate\n");
}

#[tokio::test]
async fn watcher_external_folder_racing_api_create_preserves_single_id_and_receipt() {
    use std::time::Duration;

    let rig = RuntimeRig::new_with_watch(true).await;
    let handle = rig.runtime.handle();
    let (entered, release) = handle.test_pause_external_folder();
    let notes = rig.dir.path().join("notes");
    let candidate = notes.join("Concurrent folder");
    fs::create_dir(&candidate).unwrap();
    fs::write(candidate.join("Original.md"), b"original candidate\n").unwrap();
    tokio::time::timeout(Duration::from_secs(10), entered)
        .await
        .unwrap()
        .unwrap();
    let preserved = rig.dir.path().join("preserved-candidate");
    fs::rename(&candidate, &preserved).unwrap();
    let request = CreateNode {
        epoch: rig.paired.workspace_epoch,
        stream_id: rig.paired.device_stream_id.parse().unwrap(),
        operation_seq: 1,
        operation_id: op_id(),
        node_id: node_id(),
        parent_id: rig.document.parent_id,
        name: "Concurrent folder".into(),
        kind: notryx_protocol::NodeKind::Folder,
        upload_id: None,
    };
    let receipt = handle
        .create_node(rig.paired.device_token.clone(), request.clone())
        .await
        .unwrap();
    assert!(matches!(&receipt.outcome, Outcome::Created(_)));
    let head_after_api = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    release.send(()).unwrap();
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let head = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let node = handle
        .node_state(rig.paired.device_token.clone(), request.node_id)
        .await
        .unwrap();
    let receipt_after = handle
        .operation(rig.paired.device_token.clone(), request.operation_id)
        .await
        .unwrap();
    let original = fs::read(preserved.join("Original.md")).unwrap();
    let new_folder = candidate.is_dir();
    rig.runtime.shutdown().await.unwrap();
    assert_eq!(head, head_after_api);
    assert_eq!(node.node_id, request.node_id);
    assert!(!node.deleted && new_folder);
    assert_eq!(receipt_after, receipt);
    assert_eq!(original, b"original candidate\n");
}

async fn wait_unseen_folder_and_markdown(
    handle: &crate::runtime::WorkspaceHandle,
    token: &str,
    before: notryx_domain::ChangeSeq,
    parent_id: NodeId,
) -> Option<notryx_domain::ChangeSeq> {
    use std::time::Duration;
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let head = handle.server_info(token.to_owned()).await.unwrap().head_seq;
            if head > before {
                let changes = handle
                    .changes(token.to_owned(), before, Some(head))
                    .await
                    .unwrap();
                if let Some(folder) = changes.nodes.iter().find(|node| {
                    node.parent_id == Some(parent_id)
                        && node.name == "Unseen folder"
                        && node.kind == notryx_protocol::NodeKind::Folder
                }) && changes.nodes.iter().any(|node| {
                    node.parent_id == Some(folder.node_id) && node.name == "Existing.md"
                }) {
                    break head;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .ok()
}

#[tokio::test]
async fn startup_scan_discovers_external_folder_created_while_stopped() {
    use crate::config::WorkspaceConfig;
    use crate::runtime::WorkspaceRuntime;

    let rig = RuntimeRig::new_with_watch(true).await;
    let handle = rig.runtime.handle();
    let before = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    rig.runtime.shutdown().await.unwrap();
    let notes = rig.dir.path().join("notes");
    let folder = notes.join("Unseen folder");
    fs::create_dir(&folder).unwrap();
    fs::write(folder.join("Existing.md"), b"startup folder content\n").unwrap();
    let mut runtime = WorkspaceRuntime::start(WorkspaceConfig {
        notes_root: notes,
        state_root: rig.dir.path().join("state"),
        uploads: Default::default(),
    })
    .await
    .unwrap();
    runtime.test_wait_startup_scan().await;
    let handle = runtime.handle();
    let found = wait_unseen_folder_and_markdown(
        &handle,
        &rig.paired.device_token,
        before,
        rig.document.parent_id,
    )
    .await;
    let original = fs::read(
        rig.dir
            .path()
            .join("state/objects/sha256")
            .join(&rig.document.content_hash),
    )
    .unwrap();
    let bytes = fs::read(folder.join("Existing.md")).unwrap();
    runtime.shutdown().await.unwrap();
    assert!(
        found.is_some(),
        "startup scan did not discover external folder and Markdown"
    );
    assert_eq!(found.unwrap().get(), before.get() + 2);
    assert_eq!(bytes, b"startup folder content\n");
    assert_eq!(original, b"original document\n");
}

#[tokio::test]
async fn overflow_scan_discovers_external_folder_after_missed_event() {
    let rig = RuntimeRig::new_with_watch(true).await;
    let handle = rig.runtime.handle();
    let before = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    handle.test_suppress_watch_name("Unseen folder");
    let folder = rig.dir.path().join("notes/Unseen folder");
    fs::create_dir(&folder).unwrap();
    fs::write(folder.join("Existing.md"), b"overflow folder content\n").unwrap();
    handle.test_request_watcher_overflow_scan();
    let found = wait_unseen_folder_and_markdown(
        &handle,
        &rig.paired.device_token,
        before,
        rig.document.parent_id,
    )
    .await;
    let original = fs::read(
        rig.dir
            .path()
            .join("state/objects/sha256")
            .join(&rig.document.content_hash),
    )
    .unwrap();
    let bytes = fs::read(folder.join("Existing.md")).unwrap();
    rig.runtime.shutdown().await.unwrap();
    assert!(
        found.is_some(),
        "overflow scan did not discover external folder and Markdown"
    );
    assert_eq!(found.unwrap().get(), before.get() + 2);
    assert_eq!(bytes, b"overflow folder content\n");
    assert_eq!(original, b"original document\n");
}

#[tokio::test]
async fn startup_scan_discovers_nested_external_folders_and_markdown() {
    use crate::config::WorkspaceConfig;
    use crate::runtime::WorkspaceRuntime;
    use std::time::Duration;

    let rig = RuntimeRig::new_with_watch(true).await;
    let before = rig
        .runtime
        .handle()
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    rig.runtime.shutdown().await.unwrap();
    let notes = rig.dir.path().join("notes");
    let nested = notes.join("First/Second/Third");
    fs::create_dir_all(&nested).unwrap();
    fs::write(nested.join("Leaf.md"), b"nested original\n").unwrap();
    let mut runtime = WorkspaceRuntime::start(WorkspaceConfig {
        notes_root: notes,
        state_root: rig.dir.path().join("state"),
        uploads: Default::default(),
    })
    .await
    .unwrap();
    runtime.test_wait_startup_scan().await;
    let handle = runtime.handle();
    let found = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let head = handle
                .server_info(rig.paired.device_token.clone())
                .await
                .unwrap()
                .head_seq;
            if head.get() >= before.get() + 4 {
                let changes = handle
                    .changes(rig.paired.device_token.clone(), before, Some(head))
                    .await
                    .unwrap();
                let first = changes.nodes.iter().find(|node| {
                    node.parent_id == Some(rig.document.parent_id)
                        && node.name == "First"
                        && node.kind == notryx_protocol::NodeKind::Folder
                });
                let second = first.and_then(|parent| {
                    changes.nodes.iter().find(|node| {
                        node.parent_id == Some(parent.node_id)
                            && node.name == "Second"
                            && node.kind == notryx_protocol::NodeKind::Folder
                    })
                });
                let third = second.and_then(|parent| {
                    changes.nodes.iter().find(|node| {
                        node.parent_id == Some(parent.node_id)
                            && node.name == "Third"
                            && node.kind == notryx_protocol::NodeKind::Folder
                    })
                });
                if third.is_some_and(|parent| {
                    changes.nodes.iter().any(|node| {
                        node.parent_id == Some(parent.node_id) && node.name == "Leaf.md"
                    })
                }) {
                    break head;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .ok();
    let bytes = fs::read(nested.join("Leaf.md")).unwrap();
    let original = fs::read(
        rig.dir
            .path()
            .join("state/objects/sha256")
            .join(&rig.document.content_hash),
    )
    .unwrap();
    runtime.shutdown().await.unwrap();
    assert!(
        found.is_some(),
        "nested external folders and Markdown were not discovered"
    );
    assert_eq!(found.unwrap().get(), before.get() + 4);
    assert_eq!(bytes, b"nested original\n");
    assert_eq!(original, b"original document\n");
}

#[tokio::test]
async fn startup_scan_reports_casefold_colliding_external_folder() {
    use crate::config::WorkspaceConfig;
    use crate::runtime::WorkspaceRuntime;

    let rig = RuntimeRig::new_with_watch(true).await;
    let handle = rig.runtime.handle();
    let request = CreateNode {
        epoch: rig.paired.workspace_epoch,
        stream_id: rig.paired.device_stream_id.parse().unwrap(),
        operation_seq: 1,
        operation_id: op_id(),
        node_id: node_id(),
        parent_id: rig.document.parent_id,
        name: "Foo".into(),
        kind: notryx_protocol::NodeKind::Folder,
        upload_id: None,
    };
    let receipt = handle
        .create_node(rig.paired.device_token.clone(), request.clone())
        .await
        .unwrap();
    let before = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    rig.runtime.shutdown().await.unwrap();
    let notes = rig.dir.path().join("notes");
    fs::write(notes.join("Foo/preserved.txt"), b"registered folder\n").unwrap();
    fs::create_dir(notes.join("foo")).unwrap();
    fs::write(notes.join("foo/External.md"), b"colliding folder\n").unwrap();
    let mut runtime = WorkspaceRuntime::start(WorkspaceConfig {
        notes_root: notes.clone(),
        state_root: rig.dir.path().join("state"),
        uploads: Default::default(),
    })
    .await
    .unwrap();
    runtime.test_wait_startup_scan().await;
    let handle = runtime.handle();
    let incomplete = handle.known_file_scan_incomplete();
    let head = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let node = handle
        .node_state(rig.paired.device_token.clone(), request.node_id)
        .await
        .unwrap();
    let receipt_after = handle
        .operation(rig.paired.device_token.clone(), request.operation_id)
        .await
        .unwrap();
    let registered = fs::read(notes.join("Foo/preserved.txt")).unwrap();
    let collision = fs::read(notes.join("foo/External.md")).unwrap();
    runtime.shutdown().await.unwrap();
    assert!(
        incomplete,
        "casefold-colliding external folder was silently skipped"
    );
    assert_eq!(head, before);
    assert!(!node.deleted);
    assert_eq!(receipt_after, receipt);
    assert_eq!(registered, b"registered folder\n");
    assert_eq!(collision, b"colliding folder\n");
}

#[derive(Clone, Copy)]
enum UnsupportedScanEntry {
    Symlink,
    Fifo,
    HardLink,
    ExcludedSymlink,
}

async fn check_startup_unsupported_entry(entry: UnsupportedScanEntry) {
    use crate::config::WorkspaceConfig;
    use crate::runtime::WorkspaceRuntime;
    use std::os::unix::fs::{FileTypeExt, MetadataExt, symlink};

    let rig = RuntimeRig::new_with_watch(true).await;
    let handle = rig.runtime.handle();
    let prior = SaveContent {
        epoch: rig.paired.workspace_epoch,
        stream_id: rig.paired.device_stream_id.parse().unwrap(),
        operation_seq: 1,
        operation_id: op_id(),
        node_id: rig.other.node_id,
        expected_content_rev: rig.other.content_rev.clone(),
        content_hash: digest(b"prior receipt\n"),
        bytes: b"prior receipt\n".to_vec(),
    };
    let receipt = handle
        .save_content(rig.paired.device_token.clone(), prior.clone())
        .await
        .unwrap();
    let before = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    rig.runtime.shutdown().await.unwrap();
    let notes = rig.dir.path().join("notes");
    let outside = rig.dir.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("Target.md"), b"outside bytes\n").unwrap();
    let name = match entry {
        UnsupportedScanEntry::Symlink => {
            symlink(&outside, notes.join("Linked folder")).unwrap();
            "Linked folder"
        }
        UnsupportedScanEntry::Fifo => {
            let root = fs::File::open(&notes).unwrap();
            rustix::fs::mkfifoat(
                &root,
                "Named pipe",
                rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
            )
            .unwrap();
            "Named pipe"
        }
        UnsupportedScanEntry::HardLink => {
            fs::hard_link(outside.join("Target.md"), notes.join("Hard link")).unwrap();
            "Hard link"
        }
        UnsupportedScanEntry::ExcludedSymlink => {
            symlink(&outside, notes.join(".git")).unwrap();
            ".git"
        }
    };
    fs::write(notes.join("Safe.md"), b"safe markdown\n").unwrap();
    let mut runtime = WorkspaceRuntime::start(WorkspaceConfig {
        notes_root: notes.clone(),
        state_root: rig.dir.path().join("state"),
        uploads: Default::default(),
    })
    .await
    .unwrap();
    runtime.test_wait_startup_scan().await;
    let handle = runtime.handle();
    let incomplete = handle.known_file_scan_incomplete();
    let head = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let changes = handle
        .changes(rig.paired.device_token.clone(), before, Some(head))
        .await
        .unwrap();
    let receipt_after = handle
        .operation(rig.paired.device_token.clone(), prior.operation_id)
        .await
        .unwrap();
    let entry_metadata = fs::symlink_metadata(notes.join(name)).unwrap();
    let outside_bytes = fs::read(outside.join("Target.md")).unwrap();
    let safe_bytes = fs::read(notes.join("Safe.md")).unwrap();
    let original = fs::read(
        rig.dir
            .path()
            .join("state/objects/sha256")
            .join(&rig.document.content_hash),
    )
    .unwrap();
    runtime.shutdown().await.unwrap();

    assert_eq!(
        incomplete,
        !matches!(entry, UnsupportedScanEntry::ExcludedSymlink),
        "unsupported entry was not distinguished from an excluded entry"
    );
    assert_eq!(head.get(), before.get() + 1);
    assert!(
        changes
            .nodes
            .iter()
            .any(|node| node.parent_id == Some(rig.document.parent_id) && node.name == "Safe.md")
    );
    assert!(!changes.nodes.iter().any(|node| node.name == name));
    match entry {
        UnsupportedScanEntry::Symlink | UnsupportedScanEntry::ExcludedSymlink => {
            assert!(entry_metadata.file_type().is_symlink())
        }
        UnsupportedScanEntry::Fifo => assert!(entry_metadata.file_type().is_fifo()),
        UnsupportedScanEntry::HardLink => assert_eq!(entry_metadata.nlink(), 2),
    }
    assert_eq!(receipt_after, receipt);
    assert_eq!(outside_bytes, b"outside bytes\n");
    assert_eq!(safe_bytes, b"safe markdown\n");
    assert_eq!(original, b"original document\n");
}

#[tokio::test]
async fn startup_scan_reports_non_markdown_symlink_and_continues() {
    check_startup_unsupported_entry(UnsupportedScanEntry::Symlink).await;
}

#[tokio::test]
async fn startup_scan_reports_non_markdown_fifo_and_continues() {
    check_startup_unsupported_entry(UnsupportedScanEntry::Fifo).await;
}

#[tokio::test]
async fn startup_scan_reports_non_markdown_hardlink_and_continues() {
    check_startup_unsupported_entry(UnsupportedScanEntry::HardLink).await;
}

#[tokio::test]
async fn admin_unsupported_items_includes_partial_scan_of_replaced_known_document() {
    use notryx_protocol::admin::{AdminRequest, AdminResult, UnsupportedItemReason};
    use std::os::unix::fs::symlink;
    use std::time::Duration;

    let rig = RuntimeRig::new_with_watch(true).await;
    let handle = rig.runtime.handle();
    let request = || AdminRequest::UnsupportedItems {
        snapshot_id: None,
        offset: 0,
        limit: 20,
    };
    let initial_id = match handle.admin(request()).await.unwrap() {
        AdminResult::UnsupportedItems {
            snapshot_id: Some(id),
            coverage_complete: true,
            ..
        } => id,
        _ => panic!("expected completed initial diagnostic snapshot"),
    };
    let before = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let notes = rig.dir.path().join("notes");
    let saved = rig.dir.path().join("saved-original.md");
    let outside = rig.dir.path().join("outside.md");
    fs::write(&outside, b"outside bytes\n").unwrap();
    fs::rename(notes.join("Document.md"), &saved).unwrap();
    symlink(&outside, notes.join("Document.md")).unwrap();
    let found = tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            if let AdminResult::UnsupportedItems {
                entries,
                coverage_complete,
                stale,
                previous_snapshot_id,
                ..
            } = handle.admin(request()).await.unwrap()
                && entries.iter().any(|entry| {
                    entry.parent_path.is_empty()
                        && entry.name_utf8.as_deref() == Some("Document.md")
                        && entry.reason == UnsupportedItemReason::Symlink
                })
            {
                break (coverage_complete, stale, previous_snapshot_id);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .ok();
    let previous = handle
        .admin(AdminRequest::UnsupportedItems {
            snapshot_id: Some(initial_id.clone()),
            offset: 0,
            limit: 20,
        })
        .await;
    let head = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let node = handle
        .node_state(rig.paired.device_token.clone(), rig.document.node_id)
        .await
        .unwrap();
    let source = fs::read(&saved).unwrap();
    let target = fs::read(&outside).unwrap();
    let object = fs::read(
        rig.dir
            .path()
            .join("state/objects/sha256")
            .join(&rig.document.content_hash),
    )
    .unwrap();
    rig.runtime.shutdown().await.unwrap();
    assert_eq!(
        found,
        Some((false, true, Some(initial_id))),
        "partial scan did not report the observed symlink"
    );
    assert!(matches!(
        previous,
        Ok(AdminResult::UnsupportedItems {
            coverage_complete: true,
            stale: true,
            ..
        })
    ));
    assert_eq!(head, before);
    assert!(!node.deleted);
    assert_eq!(source, b"original document\n");
    assert_eq!(target, b"outside bytes\n");
    assert_eq!(object, b"original document\n");
}

#[tokio::test]
async fn admin_unsupported_items_pages_truncated_snapshot_within_frame_limit() {
    use crate::config::WorkspaceConfig;
    use crate::runtime::WorkspaceRuntime;
    use notryx_protocol::admin::{AdminRequest, AdminResponse, AdminResult};
    use std::collections::HashSet;
    use std::os::unix::fs::symlink;

    let rig = RuntimeRig::new_with_watch(true).await;
    rig.runtime.shutdown().await.unwrap();
    let notes = rig.dir.path().join("notes");
    let outside = rig.dir.path().join("outside-target");
    fs::write(&outside, b"private target\n").unwrap();
    for index in 0..260 {
        symlink(&outside, notes.join(format!("Link-{index:03}"))).unwrap();
    }
    let mut runtime = WorkspaceRuntime::start(WorkspaceConfig {
        notes_root: notes.clone(),
        state_root: rig.dir.path().join("state"),
        uploads: Default::default(),
    })
    .await
    .unwrap();
    runtime.test_wait_startup_scan().await;
    let handle = runtime.handle();
    let mut snapshot = None;
    let mut offset = 0;
    let mut names = HashSet::new();
    loop {
        let result = handle
            .admin(AdminRequest::UnsupportedItems {
                snapshot_id: snapshot.clone(),
                offset,
                limit: 20,
            })
            .await
            .unwrap();
        let frame = serde_json::to_vec(&AdminResponse::Ok {
            result: result.clone(),
        })
        .unwrap();
        assert!(frame.len() < 16 * 1024);
        let AdminResult::UnsupportedItems {
            snapshot_id: Some(id),
            entries,
            coverage_complete,
            stale,
            truncated,
            next_offset,
            ..
        } = result
        else {
            panic!("expected snapshot page");
        };
        assert!(coverage_complete && !stale && truncated);
        if let Some(expected) = &snapshot {
            assert_eq!(&id, expected);
        } else {
            snapshot = Some(id);
        }
        assert!(entries.len() <= 20);
        for item in entries {
            assert!(item.parent_path.is_empty());
            assert!(names.insert(item.name_hex));
        }
        match next_offset {
            Some(next) => {
                assert!(next > offset);
                offset = next;
            }
            None => break,
        }
    }
    let original = fs::read(&outside).unwrap();
    let object = fs::read(
        rig.dir
            .path()
            .join("state/objects/sha256")
            .join(&rig.document.content_hash),
    )
    .unwrap();
    runtime.shutdown().await.unwrap();
    assert_eq!(names.len(), 256);
    assert_eq!(original, b"private target\n");
    assert_eq!(object, b"original document\n");
    assert!(
        fs::symlink_metadata(notes.join("Link-259"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

#[tokio::test]
async fn admin_unsupported_items_marks_stale_and_rejects_replaced_cursor() {
    use notryx_protocol::admin::{AdminRequest, AdminResult, UnsupportedItemReason};
    use std::os::unix::fs::symlink;
    use std::time::Duration;

    let rig = RuntimeRig::new_with_watch(true).await;
    let handle = rig.runtime.handle();
    let request = || AdminRequest::UnsupportedItems {
        snapshot_id: None,
        offset: 0,
        limit: 1,
    };
    let old_id = match handle.admin(request()).await.unwrap() {
        AdminResult::UnsupportedItems {
            snapshot_id: Some(id),
            coverage_complete: true,
            stale: false,
            entries,
            ..
        } if entries.is_empty() => id,
        _ => panic!("expected completed empty snapshot"),
    };
    let target = rig.dir.path().join("outside-target");
    fs::write(&target, b"outside bytes\n").unwrap();
    let signal = handle.test_signal_watch_name("Late link");
    symlink(&target, rig.dir.path().join("notes/Late link")).unwrap();
    tokio::time::timeout(Duration::from_secs(5), signal)
        .await
        .unwrap()
        .unwrap();
    let stale = handle.admin(request()).await.unwrap();
    assert!(
        matches!(stale, AdminResult::UnsupportedItems { snapshot_id: Some(ref id), stale: true, .. } if *id == old_id)
    );
    handle.test_request_watcher_overflow_scan();
    let new_id = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let AdminResult::UnsupportedItems {
                snapshot_id: Some(id),
                entries,
                coverage_complete: true,
                stale: false,
                ..
            } = handle.admin(request()).await.unwrap()
                && id != old_id
                && entries.iter().any(|item| {
                    item.name_utf8.as_deref() == Some("Late link")
                        && item.reason == UnsupportedItemReason::Symlink
                })
            {
                break id;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let old = handle
        .admin(AdminRequest::UnsupportedItems {
            snapshot_id: Some(old_id),
            offset: 0,
            limit: 1,
        })
        .await;
    let bytes = fs::read(target).unwrap();
    rig.runtime.shutdown().await.unwrap();
    assert!(!new_id.is_empty());
    assert!(matches!(
        old,
        Err(crate::runtime::RuntimeError::Workspace(
            Error::ExternalChange
        ))
    ));
    assert_eq!(bytes, b"outside bytes\n");
}

#[tokio::test]
async fn admin_unsupported_items_after_restart_is_unscanned_until_first_scan() {
    use crate::config::WorkspaceConfig;
    use crate::runtime::WorkspaceRuntime;
    use notryx_protocol::admin::{AdminRequest, AdminResult};
    use std::time::Duration;

    let rig = RuntimeRig::new_with_watch(true).await;
    let old = rig
        .runtime
        .handle()
        .admin(AdminRequest::UnsupportedItems {
            snapshot_id: None,
            offset: 0,
            limit: 20,
        })
        .await
        .unwrap();
    assert!(matches!(
        old,
        AdminResult::UnsupportedItems {
            snapshot_id: Some(_),
            coverage_complete: true,
            ..
        }
    ));
    rig.runtime.shutdown().await.unwrap();
    let (entered, entered_rx) = tokio::sync::oneshot::channel();
    let (release, release_rx) = tokio::sync::oneshot::channel();
    let mut runtime = WorkspaceRuntime::test_start_with_watch_control(
        WorkspaceConfig {
            notes_root: rig.dir.path().join("notes"),
            state_root: rig.dir.path().join("state"),
            uploads: Default::default(),
        },
        false,
        false,
        Some((entered, release_rx)),
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), entered_rx)
        .await
        .unwrap()
        .unwrap();
    let pending = runtime
        .handle()
        .admin(AdminRequest::UnsupportedItems {
            snapshot_id: None,
            offset: 0,
            limit: 20,
        })
        .await
        .unwrap();
    release.send(()).unwrap();
    runtime.test_wait_startup_scan().await;
    let completed = runtime
        .handle()
        .admin(AdminRequest::UnsupportedItems {
            snapshot_id: None,
            offset: 0,
            limit: 20,
        })
        .await
        .unwrap();
    runtime.shutdown().await.unwrap();
    assert!(matches!(
        pending,
        AdminResult::UnsupportedItems {
            snapshot_id: None,
            coverage_complete: false,
            ..
        }
    ));
    assert!(matches!(
        completed,
        AdminResult::UnsupportedItems {
            snapshot_id: Some(_),
            coverage_complete: true,
            ..
        }
    ));
}

#[tokio::test]
async fn diagnostic_only_scan_keeps_coarse_completion_and_marks_snapshot_stale() {
    use notryx_protocol::admin::{AdminRequest, AdminResult};
    use std::time::Duration;

    let rig = RuntimeRig::new_with_watch(true).await;
    let handle = rig.runtime.handle();
    assert!(!handle.known_file_scan_incomplete());
    let (entered, release) = handle.test_pause_diagnostic_scan();
    handle.test_request_diagnostic_scan();
    tokio::time::timeout(Duration::from_secs(10), entered)
        .await
        .unwrap()
        .unwrap();
    let incomplete = handle.known_file_scan_incomplete();
    let snapshot = handle
        .admin(AdminRequest::UnsupportedItems {
            snapshot_id: None,
            offset: 0,
            limit: 20,
        })
        .await
        .unwrap();
    release.send(()).unwrap();
    rig.runtime.shutdown().await.unwrap();
    assert!(!incomplete);
    assert!(matches!(
        snapshot,
        AdminResult::UnsupportedItems { stale: true, .. }
    ));
}

#[tokio::test]
async fn diagnostic_only_scan_does_not_hide_watcher_read_failure() {
    use notryx_protocol::admin::{AdminRequest, AdminResult};
    use std::time::Duration;

    let rig = RuntimeRig::new_with_watch(true).await;
    let handle = rig.runtime.handle();
    let (entered, release) = handle.test_pause_diagnostic_scan();
    handle.test_request_diagnostic_scan();
    tokio::time::timeout(Duration::from_secs(10), entered)
        .await
        .unwrap()
        .unwrap();
    let ack = handle.test_watch_read_error(true);
    tokio::time::timeout(Duration::from_secs(5), ack)
        .await
        .unwrap()
        .unwrap();
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if !matches!(
                handle
                    .admin(AdminRequest::UnsupportedItems {
                        snapshot_id: None,
                        offset: 0,
                        limit: 20
                    })
                    .await
                    .unwrap(),
                AdminResult::UnsupportedItems { stale: false, .. }
            ) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let incomplete = handle.known_file_scan_incomplete();
    rig.runtime.shutdown().await.unwrap();
    assert!(incomplete);
}

#[tokio::test]
async fn diagnostic_scan_retries_are_coalesced_and_delayed() {
    use std::time::{Duration, Instant};

    let rig = RuntimeRig::new_with_watch(true).await;
    let handle = rig.runtime.handle();
    let baseline = handle.test_diagnostic_scan_starts();
    let (entered, release) = handle.test_pause_diagnostic_scan();
    handle.test_request_diagnostic_scan();
    tokio::time::timeout(Duration::from_secs(10), entered)
        .await
        .unwrap()
        .unwrap();
    for _ in 0..30 {
        handle.test_request_diagnostic_scan();
    }
    let released_at = Instant::now();
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while handle.test_diagnostic_scan_starts() < baseline + 2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let elapsed = released_at.elapsed();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let starts = handle.test_diagnostic_scan_starts();
    rig.runtime.shutdown().await.unwrap();
    assert!(
        elapsed >= Duration::from_millis(250),
        "diagnostic retry started after {elapsed:?}"
    );
    assert_eq!(
        starts,
        baseline + 2,
        "duplicate requests started extra scans"
    );
}

#[tokio::test]
async fn diagnostic_scan_does_not_clear_pending_folder_registration() {
    use notryx_protocol::admin::{AdminRequest, AdminResult};
    use std::time::Duration;

    let rig = RuntimeRig::new_with_watch(true).await;
    let handle = rig.runtime.handle();
    let initial = match handle
        .admin(AdminRequest::UnsupportedItems {
            snapshot_id: None,
            offset: 0,
            limit: 20,
        })
        .await
        .unwrap()
    {
        AdminResult::UnsupportedItems {
            snapshot_id: Some(id),
            ..
        } => id,
        _ => panic!("expected initial snapshot"),
    };
    let (scan_entered, scan_release) = handle.test_pause_diagnostic_scan();
    handle.test_request_diagnostic_scan();
    tokio::time::timeout(Duration::from_secs(10), scan_entered)
        .await
        .unwrap()
        .unwrap();
    let (folder_entered, folder_release) = handle.test_pause_watch_folder();
    let request = CreateNode {
        epoch: rig.paired.workspace_epoch,
        stream_id: rig.paired.device_stream_id.parse().unwrap(),
        operation_seq: 1,
        operation_id: op_id(),
        node_id: node_id(),
        parent_id: rig.document.parent_id,
        name: "Pending registration".into(),
        kind: notryx_protocol::NodeKind::Folder,
        upload_id: None,
    };
    let receipt = handle
        .create_node(rig.paired.device_token.clone(), request.clone())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), folder_entered)
        .await
        .unwrap()
        .unwrap();
    assert!(handle.known_file_scan_incomplete());
    scan_release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let AdminResult::UnsupportedItems {
                snapshot_id: Some(id),
                ..
            } = handle
                .admin(AdminRequest::UnsupportedItems {
                    snapshot_id: None,
                    offset: 0,
                    limit: 20,
                })
                .await
                .unwrap()
                && id != initial
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let incomplete = handle.known_file_scan_incomplete();
    let receipt_after = handle
        .operation(rig.paired.device_token.clone(), request.operation_id)
        .await
        .unwrap();
    folder_release.send(()).unwrap();
    rig.runtime.shutdown().await.unwrap();
    assert!(
        incomplete,
        "a stale scan cleared the pending folder registration"
    );
    assert_eq!(receipt_after, receipt);
}

#[tokio::test]
async fn admin_unsupported_items_read_error_recovers_snapshot_and_incomplete_status() {
    use crate::config::WorkspaceConfig;
    use crate::runtime::WorkspaceRuntime;
    use notryx_protocol::admin::{AdminRequest, AdminResult};
    use std::os::unix::fs::symlink;
    use std::time::Duration;

    let rig = RuntimeRig::new_with_watch(true).await;
    rig.runtime.shutdown().await.unwrap();
    let notes = rig.dir.path().join("notes");
    let target = rig.dir.path().join("outside");
    fs::write(&target, b"outside bytes\n").unwrap();
    let link = notes.join("Unsupported link");
    symlink(&target, &link).unwrap();
    let mut runtime = WorkspaceRuntime::start(WorkspaceConfig {
        notes_root: notes.clone(),
        state_root: rig.dir.path().join("state"),
        uploads: Default::default(),
    })
    .await
    .unwrap();
    runtime.test_wait_startup_scan().await;
    let handle = runtime.handle();
    let request = || AdminRequest::UnsupportedItems {
        snapshot_id: None,
        offset: 0,
        limit: 20,
    };
    let initial_id = match handle.admin(request()).await.unwrap() {
        AdminResult::UnsupportedItems {
            snapshot_id: Some(id),
            coverage_complete: true,
            stale: false,
            ..
        } => id,
        _ => panic!("expected initial complete diagnostic snapshot"),
    };
    let ack = handle.test_watch_read_error(false);
    tokio::time::timeout(Duration::from_secs(5), ack)
        .await
        .unwrap()
        .unwrap();
    let failed = handle.admin(request()).await.unwrap();
    assert!(matches!(
        failed,
        AdminResult::UnsupportedItems { stale: true, .. }
    ));
    let signal = handle.test_signal_watch_name("Recovery probe.txt");
    fs::write(notes.join("Recovery probe.txt"), b"probe\n").unwrap();
    tokio::time::timeout(Duration::from_secs(5), signal)
        .await
        .unwrap()
        .unwrap();
    let recovered_id = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let AdminResult::UnsupportedItems {
                snapshot_id: Some(id),
                coverage_complete: true,
                stale: false,
                entries,
                ..
            } = handle.admin(request()).await.unwrap()
                && id != initial_id
                && entries
                    .iter()
                    .any(|entry| entry.name_utf8.as_deref() == Some("Unsupported link"))
            {
                break id;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert!(handle.known_file_scan_incomplete());
    fs::remove_file(&link).unwrap();
    handle.test_request_watcher_overflow_scan();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let AdminResult::UnsupportedItems {
                snapshot_id: Some(id),
                coverage_complete: true,
                stale: false,
                entries,
                ..
            } = handle.admin(request()).await.unwrap()
                && id != recovered_id
                && entries.is_empty()
                && !handle.known_file_scan_incomplete()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let outside_bytes = fs::read(target).unwrap();
    runtime.shutdown().await.unwrap();
    assert_eq!(outside_bytes, b"outside bytes\n");
}

#[tokio::test]
async fn startup_scan_skips_excluded_symlink_without_diagnostic() {
    check_startup_unsupported_entry(UnsupportedScanEntry::ExcludedSymlink).await;
}

#[tokio::test]
async fn watcher_folder_queue_overflow_recovers_api_created_folder() {
    use std::time::Duration;
    let rig = RuntimeRig::new_with_watch(true).await;
    let handle = rig.runtime.handle();
    let (entered, release) = handle.test_pause_watch_folder();
    let make_folder = |name: &str, sequence: u64| CreateNode {
        epoch: rig.paired.workspace_epoch,
        stream_id: rig.paired.device_stream_id.parse().unwrap(),
        operation_seq: sequence,
        operation_id: op_id(),
        node_id: node_id(),
        parent_id: rig.document.parent_id,
        name: name.into(),
        kind: notryx_protocol::NodeKind::Folder,
        upload_id: None,
    };
    let blocker = make_folder("Registration blocker", 1);
    handle
        .create_node(rig.paired.device_token.clone(), blocker)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), entered)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        handle.test_fill_watch_folder_queue(rig.document.parent_id),
        256
    );
    let target = make_folder("Queue overflow folder", 2);
    let receipt = handle
        .create_node(rig.paired.device_token.clone(), target.clone())
        .await
        .unwrap();
    let before = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let file = rig
        .dir
        .path()
        .join("notes/Queue overflow folder/External.md");
    fs::write(&file, b"queued document\n").unwrap();
    let degraded = handle.known_file_watch_degraded();
    release.send(()).unwrap();
    let found = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let head = handle
                .server_info(rig.paired.device_token.clone())
                .await
                .unwrap()
                .head_seq;
            if head > before {
                let changes = handle
                    .changes(rig.paired.device_token.clone(), before, Some(head))
                    .await
                    .unwrap();
                if changes.nodes.iter().any(|node| {
                    node.parent_id == Some(target.node_id) && node.name == "External.md"
                }) {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .is_ok();
    let receipt_after = handle
        .operation(rig.paired.device_token.clone(), target.operation_id)
        .await
        .unwrap();
    let bytes = fs::read(file).unwrap();
    rig.runtime.shutdown().await.unwrap();
    assert!(degraded, "overflow was not diagnosed");
    assert!(found, "queue overflow permanently lost folder registration");
    assert_eq!(receipt_after, receipt);
    assert_eq!(bytes, b"queued document\n");
}

#[tokio::test]
async fn watcher_folder_queue_closed_is_diagnosed_after_api_create() {
    use std::time::Duration;
    let rig = RuntimeRig::new_with_watch(true).await;
    let handle = rig.runtime.handle();
    let ack = handle.test_watch_read_error(true);
    tokio::time::timeout(Duration::from_secs(5), ack)
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !handle.test_watch_folder_queue_closed() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let events_before = handle.test_known_file_scan_events().len();
    let request = CreateNode {
        epoch: rig.paired.workspace_epoch,
        stream_id: rig.paired.device_stream_id.parse().unwrap(),
        operation_seq: 1,
        operation_id: op_id(),
        node_id: node_id(),
        parent_id: rig.document.parent_id,
        name: "Closed watcher folder".into(),
        kind: notryx_protocol::NodeKind::Folder,
        upload_id: None,
    };
    let receipt = handle
        .create_node(rig.paired.device_token.clone(), request.clone())
        .await
        .unwrap();
    let file = rig
        .dir
        .path()
        .join("notes/Closed watcher folder/External.md");
    fs::write(&file, b"preserved document\n").unwrap();
    let events = handle.test_known_file_scan_events();
    let diagnosed = events
        .iter()
        .skip(events_before)
        .any(|event| *event == "known_file_watch_registration_closed");
    let receipt_after = handle
        .operation(rig.paired.device_token.clone(), request.operation_id)
        .await
        .unwrap();
    let bytes = fs::read(file).unwrap();
    rig.runtime.shutdown().await.unwrap();
    assert!(
        diagnosed,
        "closed folder registration queue was not diagnosed"
    );
    assert_eq!(receipt_after, receipt);
    assert_eq!(bytes, b"preserved document\n");
}

#[tokio::test]
async fn watcher_recovers_second_folder_queue_overflow_during_recovery() {
    use std::time::Duration;
    let rig = RuntimeRig::new_with_watch(true).await;
    let handle = rig.runtime.handle();
    let (entered, release) = handle.test_pause_watch_folder();
    let (recovery_entered, recovery_release) = handle.test_pause_watch_recovery();
    let make_folder = |name: &str, sequence: u64| CreateNode {
        epoch: rig.paired.workspace_epoch,
        stream_id: rig.paired.device_stream_id.parse().unwrap(),
        operation_seq: sequence,
        operation_id: op_id(),
        node_id: node_id(),
        parent_id: rig.document.parent_id,
        name: name.into(),
        kind: notryx_protocol::NodeKind::Folder,
        upload_id: None,
    };
    handle
        .create_node(rig.paired.device_token.clone(), make_folder("Blocker", 1))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), entered)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        handle.test_fill_watch_folder_queue(rig.document.parent_id),
        256
    );
    let first = make_folder("Overflow one", 2);
    let receipt1 = handle
        .create_node(rig.paired.device_token.clone(), first.clone())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), recovery_entered)
        .await
        .unwrap()
        .unwrap();
    let second = make_folder("Overflow two", 3);
    let receipt2 = handle
        .create_node(rig.paired.device_token.clone(), second.clone())
        .await
        .unwrap();
    let before = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let notes = rig.dir.path().join("notes");
    fs::write(notes.join("Overflow one/First.md"), b"first\n").unwrap();
    fs::write(notes.join("Overflow two/Second.md"), b"second\n").unwrap();
    let pending = handle.known_file_scan_incomplete();
    recovery_release.send(()).unwrap();
    release.send(()).unwrap();
    let found =
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let head = handle
                    .server_info(rig.paired.device_token.clone())
                    .await
                    .unwrap()
                    .head_seq;
                if head.get() >= before.get() + 2 {
                    let changes = handle
                        .changes(rig.paired.device_token.clone(), before, Some(head))
                        .await
                        .unwrap();
                    if changes.nodes.iter().any(|node| {
                        node.parent_id == Some(first.node_id) && node.name == "First.md"
                    }) && changes.nodes.iter().any(|node| {
                        node.parent_id == Some(second.node_id) && node.name == "Second.md"
                    }) && !handle.known_file_scan_incomplete()
                    {
                        break;
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .is_ok();
    let receipt1_after = handle
        .operation(rig.paired.device_token.clone(), first.operation_id)
        .await
        .unwrap();
    let receipt2_after = handle
        .operation(rig.paired.device_token.clone(), second.operation_id)
        .await
        .unwrap();
    rig.runtime.shutdown().await.unwrap();
    assert!(
        pending,
        "recovery was reported complete while inventory was paused"
    );
    assert!(
        found,
        "repeated overflow did not recover both folders and complete the scan"
    );
    assert_eq!(receipt1_after, receipt1);
    assert_eq!(receipt2_after, receipt2);
    assert_eq!(
        fs::read(notes.join("Overflow one/First.md")).unwrap(),
        b"first\n"
    );
    assert_eq!(
        fs::read(notes.join("Overflow two/Second.md")).unwrap(),
        b"second\n"
    );
}

#[tokio::test]
async fn watcher_registers_api_folder_moved_before_watch_added() {
    use std::time::Duration;
    let rig = RuntimeRig::new_with_watch(true).await;
    let handle = rig.runtime.handle();
    let (entered, release) = handle.test_pause_watch_folder();
    let folder_id = node_id();
    let create = CreateNode {
        epoch: rig.paired.workspace_epoch,
        stream_id: rig.paired.device_stream_id.parse().unwrap(),
        operation_seq: 1,
        operation_id: op_id(),
        node_id: folder_id,
        parent_id: rig.document.parent_id,
        name: "Before move".into(),
        kind: notryx_protocol::NodeKind::Folder,
        upload_id: None,
    };
    let created = handle
        .create_node(rig.paired.device_token.clone(), create)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), entered)
        .await
        .unwrap()
        .unwrap();
    let folder = handle
        .node_state(rig.paired.device_token.clone(), folder_id)
        .await
        .unwrap();
    let moved = handle
        .move_node(
            rig.paired.device_token.clone(),
            MoveNode {
                epoch: rig.paired.workspace_epoch,
                stream_id: rig.paired.device_stream_id.parse().unwrap(),
                operation_seq: 2,
                operation_id: op_id(),
                node_id: folder_id,
                expected_meta_rev: folder.meta_rev,
                expected_parent_id: rig.document.parent_id,
                parent_id: rig.document.parent_id,
                name: "After move".into(),
            },
        )
        .await
        .unwrap();
    assert!(matches!(&moved.outcome, Outcome::Moved(_)));
    let before = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let file = rig.dir.path().join("notes/After move/External.md");
    fs::write(&file, b"moved folder document\n").unwrap();
    release.send(()).unwrap();
    let found = tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let head = handle
                .server_info(rig.paired.device_token.clone())
                .await
                .unwrap()
                .head_seq;
            if head > before {
                let changes = handle
                    .changes(rig.paired.device_token.clone(), before, Some(head))
                    .await
                    .unwrap();
                if changes
                    .nodes
                    .iter()
                    .any(|node| node.parent_id == Some(folder_id) && node.name == "External.md")
                {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .is_ok();
    let created_after = handle
        .operation(rig.paired.device_token.clone(), created.operation_id)
        .await
        .unwrap();
    let moved_after = handle
        .operation(rig.paired.device_token.clone(), moved.operation_id)
        .await
        .unwrap();
    let bytes = fs::read(file).unwrap();
    rig.runtime.shutdown().await.unwrap();
    assert!(found, "moved folder was not registered and scanned");
    assert_eq!(created_after, created);
    assert_eq!(moved_after, moved);
    assert_eq!(bytes, b"moved folder document\n");
}

#[tokio::test]
async fn watcher_skips_api_folder_deleted_before_watch_added() {
    use std::time::Duration;
    let rig = RuntimeRig::new_with_watch(true).await;
    let handle = rig.runtime.handle();
    let (entered, release) = handle.test_pause_watch_folder();
    let folder_id = node_id();
    let create = CreateNode {
        epoch: rig.paired.workspace_epoch,
        stream_id: rig.paired.device_stream_id.parse().unwrap(),
        operation_seq: 1,
        operation_id: op_id(),
        node_id: folder_id,
        parent_id: rig.document.parent_id,
        name: "Before delete".into(),
        kind: notryx_protocol::NodeKind::Folder,
        upload_id: None,
    };
    let created = handle
        .create_node(rig.paired.device_token.clone(), create)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), entered)
        .await
        .unwrap()
        .unwrap();
    let folder = handle
        .node_state(rig.paired.device_token.clone(), folder_id)
        .await
        .unwrap();
    let deleted = handle
        .delete_node(
            rig.paired.device_token.clone(),
            DeleteNode {
                epoch: rig.paired.workspace_epoch,
                stream_id: rig.paired.device_stream_id.parse().unwrap(),
                operation_seq: 2,
                operation_id: op_id(),
                node_id: folder_id,
                expected_node_etag: folder.node_etag,
            },
        )
        .await
        .unwrap();
    assert!(matches!(&deleted.outcome, Outcome::Deleted(_)));
    let jobs_before = handle.test_watch_folder_jobs_done();
    let events_before = handle.test_known_file_scan_events().len();
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while handle.test_watch_folder_jobs_done() == jobs_before {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let events = handle.test_known_file_scan_events();
    let false_failure = events
        .iter()
        .skip(events_before)
        .any(|event| *event == "known_file_watch_registration_failed");
    let node = handle
        .node_state(rig.paired.device_token.clone(), folder_id)
        .await
        .unwrap();
    let created_after = handle
        .operation(rig.paired.device_token.clone(), created.operation_id)
        .await
        .unwrap();
    let deleted_after = handle
        .operation(rig.paired.device_token.clone(), deleted.operation_id)
        .await
        .unwrap();
    let original = fs::read(rig.dir.path().join("notes/Document.md")).unwrap();
    rig.runtime.shutdown().await.unwrap();
    assert!(
        !false_failure,
        "a deleted folder was treated as a registration failure"
    );
    assert!(node.deleted);
    assert_eq!(created_after, created);
    assert_eq!(deleted_after, deleted);
    assert_eq!(original, b"original document\n");
}

#[tokio::test]
async fn watcher_reports_api_folder_replaced_before_watch_added() {
    use std::time::Duration;
    let rig = RuntimeRig::new_with_watch(true).await;
    let handle = rig.runtime.handle();
    let (entered, release) = handle.test_pause_watch_folder();
    let request = CreateNode {
        epoch: rig.paired.workspace_epoch,
        stream_id: rig.paired.device_stream_id.parse().unwrap(),
        operation_seq: 1,
        operation_id: op_id(),
        node_id: node_id(),
        parent_id: rig.document.parent_id,
        name: "Replace folder".into(),
        kind: notryx_protocol::NodeKind::Folder,
        upload_id: None,
    };
    let receipt = handle
        .create_node(rig.paired.device_token.clone(), request.clone())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), entered)
        .await
        .unwrap()
        .unwrap();
    let notes = rig.dir.path().join("notes");
    fs::rename(
        notes.join("Replace folder"),
        rig.dir.path().join("original-folder"),
    )
    .unwrap();
    fs::create_dir(notes.join("Replace folder")).unwrap();
    fs::write(notes.join("Replace folder/External.md"), b"replacement\n").unwrap();
    let jobs_before = handle.test_watch_folder_jobs_done();
    let events_before = handle.test_known_file_scan_events().len();
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while handle.test_watch_folder_jobs_done() == jobs_before {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let failed = handle
        .test_known_file_scan_events()
        .iter()
        .skip(events_before)
        .any(|event| *event == "known_file_watch_registration_failed");
    let node = handle
        .node_state(rig.paired.device_token.clone(), request.node_id)
        .await
        .unwrap();
    let receipt_after = handle
        .operation(rig.paired.device_token.clone(), request.operation_id)
        .await
        .unwrap();
    let bytes = fs::read(notes.join("Replace folder/External.md")).unwrap();
    rig.runtime.shutdown().await.unwrap();
    assert!(failed, "replaced folder was not diagnosed");
    assert!(!node.deleted);
    assert_eq!(receipt_after, receipt);
    assert_eq!(bytes, b"replacement\n");
}

#[tokio::test]
async fn external_delete_waits_for_folder_queue_overflow_recovery() {
    use std::time::Duration;
    let rig = RuntimeRig::new_with_watch(true).await;
    let handle = rig.runtime.handle();
    let (confirm_entered, confirm_release) = handle.test_pause_missing_confirmation();
    fs::remove_file(rig.dir.path().join("notes/Document.md")).unwrap();
    tokio::time::timeout(Duration::from_secs(10), confirm_entered)
        .await
        .unwrap()
        .unwrap();
    let (registration_entered, registration_release) = handle.test_pause_watch_folder();
    let make_folder = |name: &str, sequence: u64| CreateNode {
        epoch: rig.paired.workspace_epoch,
        stream_id: rig.paired.device_stream_id.parse().unwrap(),
        operation_seq: sequence,
        operation_id: op_id(),
        node_id: node_id(),
        parent_id: rig.document.parent_id,
        name: name.into(),
        kind: notryx_protocol::NodeKind::Folder,
        upload_id: None,
    };
    handle
        .create_node(rig.paired.device_token.clone(), make_folder("Blocker", 1))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), registration_entered)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        handle.test_fill_watch_folder_queue(rig.document.parent_id),
        256
    );
    let signal = handle.test_signal_watch_name("Overflow folder");
    let target = make_folder("Overflow folder", 2);
    let receipt = handle
        .create_node(rig.paired.device_token.clone(), target.clone())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), signal)
        .await
        .unwrap()
        .unwrap();
    let head = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let events_before = handle.test_known_file_scan_events().len();
    confirm_release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let info = handle
                .server_info(rig.paired.device_token.clone())
                .await
                .unwrap();
            let events = handle.test_known_file_scan_events();
            if info.head_seq != head
                || events
                    .iter()
                    .skip(events_before)
                    .any(|event| *event == "known_file_scan_incomplete")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let after = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let first = handle
        .node_state(rig.paired.device_token.clone(), rig.document.node_id)
        .await
        .unwrap();
    let incomplete = handle.known_file_scan_incomplete();
    let receipt_after = handle
        .operation(rig.paired.device_token.clone(), target.operation_id)
        .await
        .unwrap();
    let original = fs::read(
        rig.dir
            .path()
            .join("state/objects/sha256")
            .join(&rig.document.content_hash),
    )
    .unwrap();
    registration_release.send(()).unwrap();
    rig.runtime.shutdown().await.unwrap();
    assert_eq!(after, head);
    assert!(!first.deleted);
    assert!(incomplete);
    assert_eq!(receipt_after, receipt);
    assert_eq!(original, b"original document\n");
}

#[tokio::test]
async fn startup_scan_external_file_deletion_waits_then_preserves_history() {
    check_external_file_deletion(1).await;
}

#[tokio::test]
async fn overflow_scan_external_file_deletion_waits_then_preserves_history() {
    check_external_file_deletion(2).await;
}

#[tokio::test]
async fn startup_scan_multiple_absent_files_are_preserved_for_review() {
    check_multiple_external_absences(true).await;
}

#[tokio::test]
async fn watcher_multiple_absent_files_are_preserved_for_review() {
    check_multiple_external_absences(false).await;
}

#[tokio::test]
async fn external_delete_revalidates_object_after_worker_observation() {
    use std::time::Duration;
    let rig = RuntimeRig::new_with_watch(true).await;
    let handle = rig.runtime.handle();
    let before = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let (entered, release) = handle.test_pause_external_delete();
    let source = rig.dir.path().join("notes/Document.md");
    fs::remove_file(&source).unwrap();
    tokio::time::timeout(Duration::from_secs(10), entered)
        .await
        .unwrap()
        .unwrap();
    let object = rig
        .dir
        .path()
        .join("state/objects/sha256")
        .join(&rig.document.content_hash);
    let saved_object = rig.dir.path().join("saved-original-object");
    fs::rename(&object, &saved_object).unwrap();
    fs::write(&object, b"corrupt object\n").unwrap();
    release.send(()).unwrap();
    let observed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let info = handle
                .server_info(rig.paired.device_token.clone())
                .await
                .unwrap();
            if !info.ready || info.head_seq != before {
                break info;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let node = handle
        .node_state(rig.paired.device_token.clone(), rig.document.node_id)
        .await
        .unwrap();
    let original = fs::read(&saved_object).unwrap();
    let corrupt = fs::read(&object).unwrap();
    fs::remove_file(&object).unwrap();
    fs::rename(&saved_object, &object).unwrap();
    rig.runtime.shutdown().await.unwrap();
    assert_eq!(observed.head_seq, before);
    assert!(!observed.ready);
    assert!(!node.deleted);
    assert_eq!(original, b"original document\n");
    assert_eq!(corrupt, b"corrupt object\n");
}

#[tokio::test]
async fn external_delete_waiting_for_worker_preserves_second_absent_file() {
    use std::time::Duration;
    let rig = RuntimeRig::new_with_watch(true).await;
    let handle = rig.runtime.handle();
    let prior = SaveContent {
        epoch: rig.paired.workspace_epoch,
        stream_id: rig.paired.device_stream_id.parse().unwrap(),
        operation_seq: 1,
        operation_id: op_id(),
        node_id: rig.other.node_id,
        expected_content_rev: rig.other.content_rev.clone(),
        content_hash: digest(b"other saved\n"),
        bytes: b"other saved\n".to_vec(),
    };
    let receipt = handle
        .save_content(rig.paired.device_token.clone(), prior.clone())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let before = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let (entered, release) = handle.test_pause_external_delete();
    let notes = rig.dir.path().join("notes");
    fs::remove_file(notes.join("Document.md")).unwrap();
    tokio::time::timeout(Duration::from_secs(10), entered)
        .await
        .unwrap()
        .unwrap();
    let signal = handle.test_signal_watch_name("Other.md");
    fs::remove_file(notes.join("Other.md")).unwrap();
    tokio::time::timeout(Duration::from_secs(5), signal)
        .await
        .unwrap()
        .unwrap();
    let events_before = handle.test_known_file_scan_events().len();
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let events = handle.test_known_file_scan_events();
            let head = handle
                .server_info(rig.paired.device_token.clone())
                .await
                .unwrap()
                .head_seq;
            if head != before
                || events
                    .iter()
                    .skip(events_before)
                    .any(|event| *event == "known_file_scan_incomplete")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("multiple missing files were not diagnosed");
    let after = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let first = handle
        .node_state(rig.paired.device_token.clone(), rig.document.node_id)
        .await
        .unwrap();
    let second = handle
        .node_state(rig.paired.device_token.clone(), rig.other.node_id)
        .await
        .unwrap();
    let receipt_after = handle
        .operation(rig.paired.device_token.clone(), prior.operation_id)
        .await
        .unwrap();
    let first_object = fs::read(
        rig.dir
            .path()
            .join("state/objects/sha256")
            .join(&rig.document.content_hash),
    )
    .unwrap();
    let second_object = fs::read(
        rig.dir
            .path()
            .join("state/objects/sha256")
            .join(&prior.content_hash),
    )
    .unwrap();
    rig.runtime.shutdown().await.unwrap();
    assert_eq!(after, before);
    assert!(!first.deleted && !second.deleted);
    assert_eq!(receipt_after, receipt);
    assert_eq!(first_object, b"original document\n");
    assert_eq!(second_object, b"other saved\n");
}

#[tokio::test]
async fn external_delete_commit_preserves_second_file_missing_after_generation_check() {
    use std::time::Duration;
    let rig = RuntimeRig::new_with_watch(true).await;
    let handle = rig.runtime.handle();
    let before = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let (entered, release) = handle.test_pause_external_delete_commit();
    let notes = rig.dir.path().join("notes");
    fs::remove_file(notes.join("Document.md")).unwrap();
    tokio::time::timeout(Duration::from_secs(10), entered)
        .await
        .unwrap()
        .unwrap();
    let signal = handle.test_signal_watch_name("Other.md");
    fs::remove_file(notes.join("Other.md")).unwrap();
    tokio::time::timeout(Duration::from_secs(5), signal)
        .await
        .unwrap()
        .unwrap();
    let events_before = handle.test_known_file_scan_events().len();
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let info = handle
                .server_info(rig.paired.device_token.clone())
                .await
                .unwrap();
            let events = handle.test_known_file_scan_events();
            if info.head_seq != before
                || events
                    .iter()
                    .skip(events_before)
                    .any(|event| *event == "known_file_scan_incomplete")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let head = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let first = handle
        .node_state(rig.paired.device_token.clone(), rig.document.node_id)
        .await
        .unwrap();
    let second = handle
        .node_state(rig.paired.device_token.clone(), rig.other.node_id)
        .await
        .unwrap();
    let object = fs::read(
        rig.dir
            .path()
            .join("state/objects/sha256")
            .join(&rig.document.content_hash),
    )
    .unwrap();
    rig.runtime.shutdown().await.unwrap();
    assert_eq!(head, before);
    assert!(!first.deleted && !second.deleted);
    assert_eq!(object, b"original document\n");
}

#[tokio::test]
async fn external_delete_transaction_rolls_back_on_watcher_event_and_stays_ready() {
    use std::time::Duration;
    let rig = RuntimeRig::new_with_watch(true).await;
    let handle = rig.runtime.handle();
    let prior = SaveContent {
        epoch: rig.paired.workspace_epoch,
        stream_id: rig.paired.device_stream_id.parse().unwrap(),
        operation_seq: 1,
        operation_id: op_id(),
        node_id: rig.other.node_id,
        expected_content_rev: rig.other.content_rev.clone(),
        content_hash: digest(b"other saved\n"),
        bytes: b"other saved\n".to_vec(),
    };
    let receipt = handle
        .save_content(rig.paired.device_token.clone(), prior.clone())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let before = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let (entered, release) = handle.test_pause_external_delete_transaction();
    let notes = rig.dir.path().join("notes");
    fs::remove_file(notes.join("Document.md")).unwrap();
    tokio::time::timeout(Duration::from_secs(10), entered)
        .await
        .unwrap()
        .unwrap();
    let signal = handle.test_signal_watch_name("Other.md");
    fs::remove_file(notes.join("Other.md")).unwrap();
    tokio::time::timeout(Duration::from_secs(5), signal)
        .await
        .unwrap()
        .unwrap();
    let events_before = handle.test_known_file_scan_events().len();
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let info = handle
                .server_info(rig.paired.device_token.clone())
                .await
                .unwrap();
            let events = handle.test_known_file_scan_events();
            if info.head_seq != before
                || events
                    .iter()
                    .skip(events_before)
                    .any(|event| *event == "known_file_scan_incomplete")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let info = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap();
    let first = handle
        .node_state(rig.paired.device_token.clone(), rig.document.node_id)
        .await
        .unwrap();
    let second = handle
        .node_state(rig.paired.device_token.clone(), rig.other.node_id)
        .await
        .unwrap();
    let receipt_after = handle
        .operation(rig.paired.device_token.clone(), prior.operation_id)
        .await
        .unwrap();
    let object = fs::read(
        rig.dir
            .path()
            .join("state/objects/sha256")
            .join(&rig.document.content_hash),
    )
    .unwrap();
    let created = handle
        .create_node(
            rig.paired.device_token.clone(),
            CreateNode {
                epoch: rig.paired.workspace_epoch,
                stream_id: rig.paired.device_stream_id.parse().unwrap(),
                operation_seq: 2,
                operation_id: op_id(),
                node_id: node_id(),
                parent_id: rig.document.parent_id,
                name: "After rollback".into(),
                kind: notryx_protocol::NodeKind::Folder,
                upload_id: None,
            },
        )
        .await
        .unwrap();
    rig.runtime.shutdown().await.unwrap();
    assert_eq!(info.head_seq, before);
    assert!(info.ready);
    assert!(!first.deleted && !second.deleted);
    assert_eq!(receipt_after, receipt);
    assert_eq!(object, b"original document\n");
    assert!(matches!(created.outcome, Outcome::Created(_)));
}

async fn check_external_delete_during_watch_read_error(readable_error: bool) {
    use std::time::Duration;
    let rig = RuntimeRig::new_with_watch(true).await;
    let handle = rig.runtime.handle();
    let before = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let (entered, release) = handle.test_pause_external_delete();
    fs::remove_file(rig.dir.path().join("notes/Document.md")).unwrap();
    tokio::time::timeout(Duration::from_secs(10), entered)
        .await
        .unwrap()
        .unwrap();
    let ack = handle.test_watch_read_error(readable_error);
    tokio::time::timeout(Duration::from_secs(5), ack)
        .await
        .unwrap()
        .unwrap();
    let events_before = handle.test_known_file_scan_events().len();
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let head = handle
                .server_info(rig.paired.device_token.clone())
                .await
                .unwrap()
                .head_seq;
            let events = handle.test_known_file_scan_events();
            if head != before
                || events
                    .iter()
                    .skip(events_before)
                    .any(|event| *event == "known_file_scan_incomplete")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let head = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let node = handle
        .node_state(rig.paired.device_token.clone(), rig.document.node_id)
        .await
        .unwrap();
    let degraded = handle.known_file_watch_degraded();
    let object = fs::read(
        rig.dir
            .path()
            .join("state/objects/sha256")
            .join(&rig.document.content_hash),
    )
    .unwrap();
    rig.runtime.shutdown().await.unwrap();
    assert_eq!(head, before);
    assert!(!node.deleted);
    assert!(degraded);
    assert_eq!(object, b"original document\n");
}

#[tokio::test]
async fn external_delete_is_held_after_watcher_readable_error() {
    check_external_delete_during_watch_read_error(true).await;
}

#[tokio::test]
async fn external_delete_is_held_after_watcher_read_error() {
    check_external_delete_during_watch_read_error(false).await;
}

#[tokio::test]
async fn external_delete_retries_after_unrelated_watcher_event() {
    use std::time::Duration;
    let rig = RuntimeRig::new_with_watch(true).await;
    let handle = rig.runtime.handle();
    let before = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let (entered, release) = handle.test_pause_external_delete();
    let notes = rig.dir.path().join("notes");
    fs::remove_file(notes.join("Document.md")).unwrap();
    tokio::time::timeout(Duration::from_secs(10), entered)
        .await
        .unwrap()
        .unwrap();
    let signal = handle.test_signal_watch_name("Other.md");
    fs::write(notes.join("Other.md"), b"external edit\n").unwrap();
    tokio::time::timeout(Duration::from_secs(5), signal)
        .await
        .unwrap()
        .unwrap();
    let settled = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let node = handle
                .node_state(rig.paired.device_token.clone(), rig.other.node_id)
                .await
                .unwrap();
            if node.content_hash.as_deref()
                == Some(format!("sha256:{}", digest(b"external edit\n")).as_str())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    if settled.is_err() {
        let other_node = handle
            .node_state(rig.paired.device_token.clone(), rig.other.node_id)
            .await
            .unwrap();
        let active = handle.test_observation_active();
        let degraded = handle.known_file_watch_degraded();
        let events = handle.test_known_file_scan_events();
        release.send(()).unwrap();
        rig.runtime.shutdown().await.unwrap();
        panic!(
            "other file did not settle: node={other_node:?}, active={active}, degraded={degraded}, events={events:?}"
        );
    }
    release.send(()).unwrap();
    let deleted = tokio::time::timeout(Duration::from_secs(6), async {
        loop {
            let node = handle
                .node_state(rig.paired.device_token.clone(), rig.document.node_id)
                .await
                .unwrap();
            if node.deleted {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .is_ok();
    let head = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let other = fs::read(notes.join("Other.md")).unwrap();
    let object = fs::read(
        rig.dir
            .path()
            .join("state/objects/sha256")
            .join(&rig.document.content_hash),
    )
    .unwrap();
    let scan_events = handle.test_known_file_scan_events();
    let incomplete = handle.known_file_scan_incomplete();
    rig.runtime.shutdown().await.unwrap();
    assert!(
        deleted,
        "the held deletion was not retried before the 600 second sweep: head={head:?}, incomplete={incomplete}, events={scan_events:?}"
    );
    assert_eq!(head.get(), before.get() + 2);
    assert_eq!(other, b"external edit\n");
    assert_eq!(object, b"original document\n");
}

#[tokio::test]
async fn watcher_repeated_read_errors_are_rate_limited_and_shutdown() {
    use std::time::Duration;
    let rig = RuntimeRig::new_with_watch(true).await;
    let handle = rig.runtime.handle();
    handle.test_watch_inject_read_errors(10_000);
    let source = rig.dir.path().join("notes/read-error-probe.txt");
    fs::write(&source, b"probe\n").unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while handle.test_watch_observed_read_errors() == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let observed = handle.test_watch_observed_read_errors();
    let degraded = handle.known_file_watch_degraded();
    tokio::time::timeout(Duration::from_secs(3), rig.runtime.shutdown())
        .await
        .unwrap()
        .unwrap();
    assert!(degraded);
    assert!(
        observed <= 4,
        "watcher retried read errors {observed} times in 300ms"
    );
    assert_eq!(fs::read(source).unwrap(), b"probe\n");
}

#[tokio::test]
async fn watcher_read_error_keeps_scan_incomplete_until_recovery_scan() {
    use std::time::Duration;
    let rig = RuntimeRig::new_with_watch(true).await;
    let handle = rig.runtime.handle();
    let (entered, release) = handle.test_pause_scan_finish();
    let events_before = handle.test_known_file_scan_events().len();
    handle.test_request_watcher_overflow_scan();
    tokio::time::timeout(Duration::from_secs(10), entered)
        .await
        .unwrap()
        .unwrap();
    let ack = handle.test_watch_read_error(false);
    tokio::time::timeout(Duration::from_secs(5), ack)
        .await
        .unwrap()
        .unwrap();
    let during_error = handle.known_file_scan_incomplete();
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let events = handle.test_known_file_scan_events();
            if events.iter().skip(events_before).any(|event| {
                *event == "known_file_scan_complete" || *event == "known_file_scan_incomplete"
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let while_unavailable = handle.known_file_scan_incomplete();
    let scan_guard = handle.test_lock_known_file_scan().await;
    let (recovered, resume_watch) = handle.test_pause_watch_recovered();
    let signal = handle.test_signal_watch_name("read-recovery.txt");
    let probe = rig.dir.path().join("notes/read-recovery.txt");
    fs::write(&probe, b"probe\n").unwrap();
    tokio::time::timeout(Duration::from_secs(5), signal)
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), recovered)
        .await
        .unwrap()
        .unwrap();
    let before_recovery_scan = handle.known_file_scan_incomplete();
    resume_watch.send(()).unwrap();
    drop(scan_guard);
    tokio::time::timeout(Duration::from_secs(5), async {
        while handle.known_file_scan_incomplete() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    rig.runtime.shutdown().await.unwrap();
    assert!(
        during_error && while_unavailable && before_recovery_scan,
        "incomplete cleared before recovery scan"
    );
    assert_eq!(fs::read(probe).unwrap(), b"probe\n");
}

#[tokio::test]
async fn external_delete_waits_until_watch_read_queue_is_drained() {
    use std::time::Duration;
    let rig = RuntimeRig::new_with_watch(true).await;
    let handle = rig.runtime.handle();
    let before = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let notes = rig.dir.path().join("notes");
    let (delete_entered, delete_release) = handle.test_pause_external_delete();
    handle.test_watch_inject_read_errors(3);
    fs::write(notes.join("read-error-probe.txt"), b"probe\n").unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while handle.test_watch_observed_read_errors() == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    fs::remove_file(notes.join("Document.md")).unwrap();
    handle.test_request_watcher_overflow_scan();
    tokio::time::timeout(Duration::from_secs(10), delete_entered)
        .await
        .unwrap()
        .unwrap();
    let delete_generation = handle.test_watch_generation();
    tokio::time::timeout(Duration::from_secs(5), async {
        while handle.test_watch_observed_read_errors() < 3 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        handle.test_watch_generation(),
        delete_generation,
        "read errors must finish before the delete observation"
    );
    let (read_entered, read_release) = handle.test_pause_next_watch_read();
    for i in 0..150 {
        fs::write(notes.join(format!("event-{i}.txt")), b"event\n").unwrap();
    }
    fs::remove_file(notes.join("Other.md")).unwrap();
    tokio::time::timeout(Duration::from_secs(10), read_entered)
        .await
        .unwrap()
        .unwrap();
    let events_before = handle.test_known_file_scan_events().len();
    delete_release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let info = handle
                .server_info(rig.paired.device_token.clone())
                .await
                .unwrap();
            let events = handle.test_known_file_scan_events();
            if info.head_seq != before
                || events
                    .iter()
                    .skip(events_before)
                    .any(|event| *event == "known_file_scan_incomplete")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let head = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let first = handle
        .node_state(rig.paired.device_token.clone(), rig.document.node_id)
        .await
        .unwrap();
    let second = handle
        .node_state(rig.paired.device_token.clone(), rig.other.node_id)
        .await
        .unwrap();
    let object = fs::read(
        rig.dir
            .path()
            .join("state/objects/sha256")
            .join(&rig.document.content_hash),
    )
    .unwrap();
    read_release.send(()).unwrap();
    rig.runtime.shutdown().await.unwrap();
    assert_eq!(head, before);
    assert!(!first.deleted && !second.deleted);
    assert_eq!(object, b"original document\n");
}

#[tokio::test]
async fn external_delete_retries_when_missing_confirmation_errors_after_event() {
    use std::time::Duration;
    let rig = RuntimeRig::new_with_watch(true).await;
    let handle = rig.runtime.handle();
    let before = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let (entered, release) = handle.test_confirm_missing_fault(rig.other.node_id);
    let notes = rig.dir.path().join("notes");
    fs::remove_file(notes.join("Document.md")).unwrap();
    tokio::time::timeout(Duration::from_secs(10), entered)
        .await
        .unwrap()
        .unwrap();
    let signal = handle.test_signal_watch_name("Other.md");
    fs::write(notes.join("Other.md"), b"external edit\n").unwrap();
    tokio::time::timeout(Duration::from_secs(5), signal)
        .await
        .unwrap()
        .unwrap();
    release.send(()).unwrap();
    let converged = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let first = handle
                .node_state(rig.paired.device_token.clone(), rig.document.node_id)
                .await
                .unwrap();
            let second = handle
                .node_state(rig.paired.device_token.clone(), rig.other.node_id)
                .await
                .unwrap();
            if first.deleted
                && second.content_hash.as_deref()
                    == Some(format!("sha256:{}", digest(b"external edit\n")).as_str())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .is_ok();
    let head = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let events = handle.test_known_file_scan_events();
    let original = fs::read(
        rig.dir
            .path()
            .join("state/objects/sha256")
            .join(&rig.document.content_hash),
    )
    .unwrap();
    rig.runtime.shutdown().await.unwrap();
    assert!(
        converged,
        "confirmation error was not retried: events={events:?}"
    );
    assert_eq!(head.get(), before.get() + 2);
    assert_eq!(original, b"original document\n");
    assert_eq!(
        fs::read(notes.join("Other.md")).unwrap(),
        b"external edit\n"
    );
}

async fn check_multiple_external_absences(startup: bool) {
    use crate::config::WorkspaceConfig;
    use crate::runtime::WorkspaceRuntime;
    use std::time::Duration;
    let rig = RuntimeRig::new_with_watch(true).await;
    let handle = rig.runtime.handle();
    let prior = SaveContent {
        epoch: rig.paired.workspace_epoch,
        stream_id: rig.paired.device_stream_id.parse().unwrap(),
        operation_seq: 1,
        operation_id: op_id(),
        node_id: rig.other.node_id,
        expected_content_rev: rig.other.content_rev.clone(),
        content_hash: digest(b"other saved\n"),
        bytes: b"other saved\n".to_vec(),
    };
    let receipt = handle
        .save_content(rig.paired.device_token.clone(), prior.clone())
        .await
        .unwrap();
    let before = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let RuntimeRig {
        dir,
        runtime,
        document,
        other,
        paired,
        ..
    } = rig;
    let notes = dir.path().join("notes");
    let state = dir.path().join("state");
    let mut runtime = if startup {
        runtime.shutdown().await.unwrap();
        fs::remove_file(notes.join("Document.md")).unwrap();
        fs::remove_file(notes.join("Other.md")).unwrap();
        WorkspaceRuntime::start(WorkspaceConfig {
            notes_root: notes.clone(),
            state_root: state.clone(),
            uploads: Default::default(),
        })
        .await
        .unwrap()
    } else {
        fs::remove_file(notes.join("Document.md")).unwrap();
        fs::remove_file(notes.join("Other.md")).unwrap();
        runtime
    };
    if startup {
        tokio::time::timeout(Duration::from_secs(10), runtime.test_wait_startup_scan())
            .await
            .unwrap();
    } else {
        tokio::time::sleep(Duration::from_millis(2600)).await;
    }
    let handle = runtime.handle();
    let after = handle
        .server_info(paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let first = handle
        .node_state(paired.device_token.clone(), document.node_id)
        .await
        .unwrap();
    let second = handle
        .node_state(paired.device_token.clone(), other.node_id)
        .await
        .unwrap();
    let receipt_after = handle
        .operation(paired.device_token.clone(), prior.operation_id)
        .await
        .unwrap();
    let diagnosed = handle.known_file_scan_incomplete() || handle.known_file_watch_degraded();
    let first_object = fs::read(state.join("objects/sha256").join(&document.content_hash)).unwrap();
    let second_object = fs::read(state.join("objects/sha256").join(&prior.content_hash)).unwrap();
    runtime.shutdown().await.unwrap();
    assert_eq!(after, before);
    assert!(!first.deleted && !second.deleted);
    assert_eq!(receipt_after, receipt);
    assert!(
        diagnosed,
        "multiple missing files must remain visible as incomplete or degraded"
    );
    assert_eq!(first_object, b"original document\n");
    assert_eq!(second_object, b"other saved\n");
    assert!(!notes.join("Document.md").exists());
    assert!(!notes.join("Other.md").exists());
}

async fn check_external_file_deletion(mode: u8) {
    use crate::config::WorkspaceConfig;
    use crate::runtime::WorkspaceRuntime;
    use std::time::Duration;
    let rig = RuntimeRig::new_with_watch(true).await;
    let handle = rig.runtime.handle();
    let prior = SaveContent {
        epoch: rig.paired.workspace_epoch,
        stream_id: rig.paired.device_stream_id.parse().unwrap(),
        operation_seq: 1,
        operation_id: op_id(),
        node_id: rig.other.node_id,
        expected_content_rev: rig.other.content_rev.clone(),
        content_hash: digest(b"prior receipt\n"),
        bytes: b"prior receipt\n".to_vec(),
    };
    let receipt = handle
        .save_content(rig.paired.device_token.clone(), prior.clone())
        .await
        .unwrap();
    let before = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let parent_before = handle
        .node_state(rig.paired.device_token.clone(), rig.document.parent_id)
        .await
        .unwrap();
    let RuntimeRig {
        dir,
        runtime,
        document,
        paired,
        ..
    } = rig;
    let notes = dir.path().join("notes");
    let state = dir.path().join("state");
    let source = notes.join("Document.md");
    let runtime = if mode == 1 {
        runtime.shutdown().await.unwrap();
        fs::remove_file(&source).unwrap();
        WorkspaceRuntime::start(WorkspaceConfig {
            notes_root: notes.clone(),
            state_root: state.clone(),
            uploads: Default::default(),
        })
        .await
        .unwrap()
    } else {
        if mode == 2 {
            handle.test_suppress_watch_name("Document.md");
        }
        fs::remove_file(&source).unwrap();
        if mode == 2 {
            handle.test_request_watcher_overflow_scan();
        }
        runtime
    };
    let handle = runtime.handle();
    tokio::time::sleep(Duration::from_millis(600)).await;
    let early = handle
        .server_info(paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let observed = tokio::time::timeout(Duration::from_secs(7), async {
        loop {
            let head = handle
                .server_info(paired.device_token.clone())
                .await
                .unwrap()
                .head_seq;
            if head != before {
                break head;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    let outcome = if let Ok(head) = observed {
        let page = handle
            .changes(paired.device_token.clone(), before, Some(head))
            .await
            .unwrap();
        let node = handle
            .node_state(paired.device_token.clone(), document.node_id)
            .await
            .unwrap();
        let parent = handle
            .node_state(paired.device_token.clone(), document.parent_id)
            .await
            .unwrap();
        Some((head, page, node, parent))
    } else {
        None
    };
    let receipt_after = handle
        .operation(paired.device_token.clone(), prior.operation_id)
        .await
        .unwrap();
    let object = fs::read(state.join("objects/sha256").join(&document.content_hash)).unwrap();
    runtime.shutdown().await.unwrap();
    assert_eq!(
        early, before,
        "deletion must not commit before the grace period"
    );
    let (head, page, node, parent) = outcome.expect("external deletion was not recorded");
    assert_eq!(head.get(), before.get() + 1);
    assert!(node.deleted);
    assert_eq!(node.node_id, document.node_id);
    assert_eq!(page.nodes.len(), 2);
    assert!(
        page.nodes
            .iter()
            .any(|node| node.node_id == document.node_id && node.deleted)
    );
    assert!(
        page.nodes
            .iter()
            .any(|node| node.node_id == document.parent_id)
    );
    assert_eq!(
        parent.children_generation.get(),
        parent_before.children_generation.get() + 1
    );
    assert_eq!(parent.meta_rev, parent_before.meta_rev);
    assert_eq!(receipt_after, receipt);
    assert_eq!(object, b"original document\n");
    assert!(!source.exists());
}

#[tokio::test]
async fn external_file_recreated_during_grace_is_not_deleted() {
    check_recreated_file_during_grace(false).await;
}

#[tokio::test]
async fn external_atomic_replace_during_grace_is_not_deleted() {
    check_recreated_file_during_grace(true).await;
}

async fn check_recreated_file_during_grace(atomic: bool) {
    use std::time::Duration;
    let rig = RuntimeRig::new_with_watch(true).await;
    let handle = rig.runtime.handle();
    let before = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let source = rig.dir.path().join("notes/Document.md");
    let old_object = rig
        .dir
        .path()
        .join("state/objects/sha256")
        .join(&rig.document.content_hash);
    fs::remove_file(&source).unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let external = b"recreated content\n";
    if atomic {
        let temporary = rig.dir.path().join("notes/replacement.tmp");
        fs::write(&temporary, external).unwrap();
        fs::rename(temporary, &source).unwrap();
    } else {
        fs::write(&source, external).unwrap();
    }
    let head = tokio::time::timeout(Duration::from_secs(7), async {
        loop {
            let head = handle
                .server_info(rig.paired.device_token.clone())
                .await
                .unwrap()
                .head_seq;
            if head != before {
                break head;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let node = handle
        .node_state(rig.paired.device_token.clone(), rig.document.node_id)
        .await
        .unwrap();
    let after = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let content = handle
        .content(rig.paired.device_token.clone(), rig.document.node_id, None)
        .await
        .ok();
    let prior = fs::read(old_object).unwrap();
    rig.runtime.shutdown().await.unwrap();
    assert_eq!(head.unwrap().get(), before.get() + 1);
    assert_eq!(after.get(), before.get() + 1);
    assert!(!node.deleted);
    assert_eq!(node.node_id, rig.document.node_id);
    assert_eq!(content.unwrap().1, external);
    assert_eq!(fs::read(source).unwrap(), external);
    assert_eq!(prior, b"original document\n");
}

#[tokio::test]
async fn external_deletion_does_not_tombstone_when_parent_cannot_be_opened() {
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;
    let (rig, folder) = RuntimeRig::new_with_registered_folder_watch().await;
    let handle = rig.runtime.handle();
    let request = MoveNode {
        epoch: rig.paired.workspace_epoch,
        stream_id: rig.paired.device_stream_id.parse().unwrap(),
        operation_seq: 1,
        operation_id: op_id(),
        node_id: rig.document.node_id,
        expected_meta_rev: rig.document.meta_rev.clone(),
        expected_parent_id: rig.document.parent_id,
        parent_id: folder,
        name: "Document.md".into(),
    };
    let receipt = handle
        .move_node(rig.paired.device_token.clone(), request.clone())
        .await
        .unwrap();
    assert!(matches!(&receipt.outcome, Outcome::Moved(_)));
    let before = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let source = rig.dir.path().join("notes/Folder/Document.md");
    let directory = rig.dir.path().join("notes/Folder");
    fs::remove_file(&source).unwrap();
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o0)).unwrap();
    if fs::read_dir(&directory).is_ok() {
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        rig.runtime.shutdown().await.unwrap();
        eprintln!("permission-error fixture unsupported for this process");
        return;
    }
    tokio::time::sleep(Duration::from_millis(1800)).await;
    let after = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let node = handle
        .node_state(rig.paired.device_token.clone(), rig.document.node_id)
        .await
        .unwrap();
    let receipt_after = handle
        .operation(rig.paired.device_token.clone(), request.operation_id)
        .await
        .unwrap();
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
    let object = fs::read(
        rig.dir
            .path()
            .join("state/objects/sha256")
            .join(&rig.document.content_hash),
    )
    .unwrap();
    rig.runtime.shutdown().await.unwrap();
    assert_eq!(after, before);
    assert!(!node.deleted);
    assert_eq!(receipt_after, receipt);
    assert_eq!(object, b"original document\n");
}

async fn check_startup_new_file_scan_continues_after_invalid_name(invalid_name: &std::ffi::OsStr) {
    use crate::config::WorkspaceConfig;
    use crate::runtime::WorkspaceRuntime;

    let (rig, folder) = RuntimeRig::new_with_registered_folder_watch().await;
    let RuntimeRig {
        dir,
        runtime,
        paired,
        ..
    } = rig;
    let before = runtime
        .handle()
        .server_info(paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    runtime.shutdown().await.unwrap();
    let notes = dir.path().join("notes");
    let state = dir.path().join("state");
    let earlier = notes.join("Folder/Before.md");
    fs::write(&earlier, b"earlier new document\n").unwrap();
    let unsupported = notes.join("Folder").join(invalid_name);
    fs::write(&unsupported, b"unsupported filename bytes\n").unwrap();
    let source = notes.join("Folder/New.md");
    fs::write(&source, b"new document\n").unwrap();
    let mut runtime = WorkspaceRuntime::start(WorkspaceConfig {
        notes_root: notes.clone(),
        state_root: state,
        uploads: Default::default(),
    })
    .await
    .unwrap();
    runtime.test_wait_startup_scan().await;
    let handle = runtime.handle();
    let head = handle
        .server_info(paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let page = handle
        .changes(paired.device_token.clone(), before, Some(head))
        .await
        .unwrap();
    let incomplete = handle.known_file_scan_incomplete();
    runtime.shutdown().await.unwrap();
    assert_eq!(head.get(), before.get() + 2);
    assert!(
        page.nodes
            .iter()
            .any(|node| node.name == "New.md" && node.parent_id == Some(folder))
    );
    assert!(
        page.nodes
            .iter()
            .any(|node| node.name == "Before.md" && node.parent_id == Some(folder))
    );
    assert!(incomplete, "unsupported filename must remain diagnosed");
    assert_eq!(
        fs::read(unsupported).unwrap(),
        b"unsupported filename bytes\n"
    );
    assert_eq!(fs::read(source).unwrap(), b"new document\n");
    assert_eq!(fs::read(earlier).unwrap(), b"earlier new document\n");
}

#[tokio::test]
async fn new_file_scan_bounds_queue_and_shutdown_preserves_files() {
    use crate::config::WorkspaceConfig;
    use crate::runtime::WorkspaceRuntime;
    use std::time::Duration;
    let (rig, _) = RuntimeRig::new_with_registered_folder_watch().await;
    let RuntimeRig {
        dir,
        runtime,
        document,
        paired,
        ..
    } = rig;
    let before = runtime
        .handle()
        .server_info(paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    runtime.shutdown().await.unwrap();
    let notes = dir.path().join("notes");
    let state = dir.path().join("state");
    for index in 0..80 {
        fs::write(
            notes.join("Folder").join(format!("Bulk-{index:03}.md")),
            format!("document {index}\n"),
        )
        .unwrap();
    }
    let runtime = WorkspaceRuntime::start(WorkspaceConfig {
        notes_root: notes.clone(),
        state_root: state.clone(),
        uploads: Default::default(),
    })
    .await
    .unwrap();
    let handle = runtime.handle();
    tokio::time::timeout(Duration::from_secs(10), async {
        while handle.test_new_file_scan_max_buffer() < 8 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the bounded channel never experienced backpressure");
    let maximum = handle.test_new_file_scan_max_buffer();
    tokio::time::timeout(Duration::from_secs(10), runtime.shutdown())
        .await
        .expect("shutdown did not drain scan")
        .unwrap();
    assert!(maximum <= 8);
    for index in 0..80 {
        assert_eq!(
            fs::read(notes.join("Folder").join(format!("Bulk-{index:03}.md"))).unwrap(),
            format!("document {index}\n").as_bytes()
        );
    }
    assert_eq!(
        fs::read(state.join("objects/sha256").join(&document.content_hash)).unwrap(),
        b"original document\n"
    );
    let mut reopened = Workspace::open(&notes, &state).await.unwrap();
    let head = reopened.state.head_seq().await.unwrap();
    assert!(head >= before && head.get() <= before.get() + 80);
    assert!(reopened.state.pending().await.unwrap().is_empty());
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn new_file_scan_rejects_parent_replaced_after_lookup() {
    use std::time::Duration;
    let (rig, _) = RuntimeRig::new_with_registered_folder_watch().await;
    let handle = rig.runtime.handle();
    let before = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    handle.test_suppress_watch_name("Scan.md");
    let (entered, release, done) = handle.test_pause_new_file_scan();
    let notes = rig.dir.path().join("notes");
    fs::write(notes.join("Folder/Scan.md"), b"old parent file\n").unwrap();
    handle.test_request_watcher_overflow_scan();
    tokio::time::timeout(Duration::from_secs(10), entered)
        .await
        .unwrap()
        .unwrap();
    let original = rig.dir.path().join("original-folder");
    fs::rename(notes.join("Folder"), &original).unwrap();
    fs::create_dir(notes.join("Folder")).unwrap();
    fs::write(notes.join("Folder/Scan.md"), b"replacement parent file\n").unwrap();
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), done)
        .await
        .unwrap()
        .unwrap();
    let after = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    rig.runtime.shutdown().await.unwrap();
    assert_eq!(after, before);
    assert_eq!(
        fs::read(original.join("Scan.md")).unwrap(),
        b"old parent file\n"
    );
    assert_eq!(
        fs::read(notes.join("Folder/Scan.md")).unwrap(),
        b"replacement parent file\n"
    );
}

#[tokio::test]
async fn startup_new_file_scan_rejects_symlink_and_preserves_target() {
    use crate::config::WorkspaceConfig;
    use crate::runtime::WorkspaceRuntime;
    use std::os::unix::fs::symlink;
    let (rig, folder) = RuntimeRig::new_with_registered_folder_watch().await;
    let RuntimeRig {
        dir,
        runtime,
        paired,
        ..
    } = rig;
    let handle = runtime.handle();
    let before = handle
        .server_info(paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let parent_before = handle
        .node_state(paired.device_token.clone(), folder)
        .await
        .unwrap();
    runtime.shutdown().await.unwrap();
    let notes = dir.path().join("notes");
    let state = dir.path().join("state");
    let target = dir.path().join("outside.md");
    fs::write(&target, b"outside protected\n").unwrap();
    let link = notes.join("Folder/Link.md");
    symlink(&target, &link).unwrap();
    let mut runtime = WorkspaceRuntime::start(WorkspaceConfig {
        notes_root: notes,
        state_root: state,
        uploads: Default::default(),
    })
    .await
    .unwrap();
    runtime.test_wait_startup_scan().await;
    let handle = runtime.handle();
    let after = handle
        .server_info(paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let parent_after = handle
        .node_state(paired.device_token.clone(), folder)
        .await
        .unwrap();
    let incomplete = handle.known_file_scan_incomplete();
    runtime.shutdown().await.unwrap();
    assert_eq!(after, before);
    assert_eq!(parent_after, parent_before);
    assert!(incomplete);
    assert!(fs::symlink_metadata(link).unwrap().file_type().is_symlink());
    assert_eq!(fs::read(target).unwrap(), b"outside protected\n");
}

#[tokio::test]
async fn new_file_scan_racing_api_create_preserves_single_node_and_receipt() {
    use std::time::Duration;
    let (rig, folder) = RuntimeRig::new_with_registered_folder_watch().await;
    let handle = rig.runtime.handle();
    let before = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    handle.test_suppress_watch_name("Scan.md");
    let (entered, release, done) = handle.test_pause_new_file_scan();
    let source = rig.dir.path().join("notes/Folder/Scan.md");
    fs::write(&source, b"transient scan file\n").unwrap();
    handle.test_request_watcher_overflow_scan();
    tokio::time::timeout(Duration::from_secs(10), entered)
        .await
        .unwrap()
        .unwrap();
    fs::remove_file(&source).unwrap();
    let request = CreateNode {
        epoch: rig.paired.workspace_epoch,
        stream_id: rig.paired.device_stream_id.parse().unwrap(),
        operation_seq: 1,
        operation_id: op_id(),
        node_id: node_id(),
        parent_id: folder,
        name: "Scan.md".into(),
        kind: notryx_protocol::NodeKind::File,
        upload_id: None,
    };
    let receipt = handle
        .create_node(rig.paired.device_token.clone(), request.clone())
        .await
        .unwrap();
    assert!(matches!(&receipt.outcome, Outcome::Created(_)));
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), done)
        .await
        .unwrap()
        .unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;
    let after = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let page = handle
        .changes(rig.paired.device_token.clone(), before, Some(after))
        .await
        .unwrap();
    let receipt_after = handle
        .operation(rig.paired.device_token.clone(), request.operation_id)
        .await
        .unwrap();
    let bytes = handle
        .content(rig.paired.device_token.clone(), request.node_id, None)
        .await
        .unwrap()
        .1;
    rig.runtime.shutdown().await.unwrap();
    assert_eq!(after.get(), before.get() + 1);
    assert_eq!(
        page.nodes
            .iter()
            .filter(|node| node.parent_id == Some(folder) && node.name == "Scan.md")
            .count(),
        1
    );
    assert_eq!(receipt_after, receipt);
    assert!(bytes.is_empty());
    assert!(fs::read(source).unwrap().is_empty());
}

async fn check_registered_folder_scan_discovers_new_markdown(startup: bool) {
    use crate::config::WorkspaceConfig;
    use crate::runtime::WorkspaceRuntime;
    use std::time::Duration;

    let (rig, folder) = RuntimeRig::new_with_registered_folder_watch().await;
    let handle = rig.runtime.handle();
    let prior = SaveContent {
        epoch: rig.paired.workspace_epoch,
        stream_id: rig.paired.device_stream_id.parse().unwrap(),
        operation_seq: 1,
        operation_id: op_id(),
        node_id: rig.other.node_id,
        expected_content_rev: rig.other.content_rev.clone(),
        content_hash: digest(b"prior receipt\n"),
        bytes: b"prior receipt\n".to_vec(),
    };
    let receipt = handle
        .save_content(rig.paired.device_token.clone(), prior.clone())
        .await
        .unwrap();
    let before = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let parent_before = handle
        .node_state(rig.paired.device_token.clone(), folder)
        .await
        .unwrap();
    let RuntimeRig {
        dir,
        runtime,
        document,
        paired,
        ..
    } = rig;
    let notes = dir.path().join("notes");
    let state = dir.path().join("state");
    let source = notes.join("Folder/Offline.md");
    let external = b"\xef\xbb\xbf# discovered by scan\r\n";
    let runtime = if startup {
        runtime.shutdown().await.unwrap();
        fs::write(&source, external).unwrap();
        let mut reopened = WorkspaceRuntime::start(WorkspaceConfig {
            notes_root: notes.clone(),
            state_root: state.clone(),
            uploads: Default::default(),
        })
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(10), reopened.test_wait_startup_scan())
            .await
            .unwrap();
        reopened
    } else {
        handle.test_suppress_watch_name("Offline.md");
        let events_before = handle.test_known_file_scan_events().len();
        fs::write(&source, external).unwrap();
        handle.test_request_watcher_overflow_scan();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let events = handle.test_known_file_scan_events();
                if events.iter().skip(events_before).any(|event| {
                    matches!(
                        *event,
                        "known_file_scan_complete" | "known_file_scan_incomplete"
                    )
                }) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("overflow scan did not finish");
        runtime
    };
    let handle = runtime.handle();
    let after = handle
        .server_info(paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let parent_after = handle
        .node_state(paired.device_token.clone(), folder)
        .await
        .unwrap();
    let page = handle
        .changes(paired.device_token.clone(), before, Some(after))
        .await
        .unwrap();
    let receipt_after = handle
        .operation(paired.device_token.clone(), prior.operation_id)
        .await
        .unwrap();
    let discovered = page
        .nodes
        .iter()
        .find(|node| node.parent_id == Some(folder) && node.name == "Offline.md")
        .cloned();
    let bytes = match &discovered {
        Some(node) => Some(
            handle
                .content(paired.device_token.clone(), node.node_id, None)
                .await
                .unwrap()
                .1,
        ),
        None => None,
    };
    let original = fs::read(notes.join("Document.md")).unwrap();
    let object = fs::read(state.join("objects/sha256").join(&document.content_hash)).unwrap();
    let incomplete = handle.known_file_scan_incomplete();
    runtime.shutdown().await.unwrap();
    assert_eq!(
        after.get(),
        before.get() + 1,
        "scan must discover the new document"
    );
    assert!(!incomplete);
    assert_eq!(page.nodes.len(), 2);
    assert!(page.nodes.iter().any(|node| node.node_id == folder));
    assert!(discovered.is_some());
    assert_eq!(bytes.unwrap(), external);
    assert_eq!(
        parent_after.children_generation.get(),
        parent_before.children_generation.get() + 1
    );
    assert_eq!(parent_after.meta_rev, parent_before.meta_rev);
    assert_eq!(receipt_after, receipt);
    assert_eq!(original, b"original document\n");
    assert_eq!(object, b"original document\n");
    assert_eq!(fs::read(source).unwrap(), external);
}

#[tokio::test]
async fn watcher_new_markdown_symlink_preserves_target_and_creates_no_node() {
    use std::os::unix::fs::symlink;
    use std::time::Duration;
    let (rig, folder) = RuntimeRig::new_with_registered_folder_watch().await;
    let handle = rig.runtime.handle();
    let before = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let parent_before = handle
        .node_state(rig.paired.device_token.clone(), folder)
        .await
        .unwrap();
    let target = rig.dir.path().join("outside.md");
    fs::write(&target, b"outside protected\n").unwrap();
    let link = rig.dir.path().join("notes/Folder/Link.md");
    symlink(&target, &link).unwrap();
    let degraded = tokio::time::timeout(Duration::from_secs(5), async {
        while !handle.known_file_watch_degraded() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .is_ok();
    let after = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let parent_after = handle
        .node_state(rig.paired.device_token.clone(), folder)
        .await
        .unwrap();
    rig.runtime.shutdown().await.unwrap();
    assert!(degraded);
    assert_eq!(after, before);
    assert_eq!(parent_after, parent_before);
    assert!(fs::symlink_metadata(link).unwrap().file_type().is_symlink());
    assert_eq!(fs::read(target).unwrap(), b"outside protected\n");
}

#[tokio::test]
async fn watcher_new_markdown_rejects_parent_replaced_after_lookup() {
    use std::time::Duration;
    let (rig, _) = RuntimeRig::new_with_registered_folder_watch().await;
    let handle = rig.runtime.handle();
    let before = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let (entered, release, done) = handle.test_pause_watch_lookup();
    let notes = rig.dir.path().join("notes");
    fs::write(notes.join("Folder/New.md"), b"original folder new file\n").unwrap();
    tokio::time::timeout(Duration::from_secs(10), entered)
        .await
        .unwrap()
        .unwrap();
    let original = rig.dir.path().join("old-folder");
    fs::rename(notes.join("Folder"), &original).unwrap();
    fs::create_dir(notes.join("Folder")).unwrap();
    fs::write(
        notes.join("Folder/New.md"),
        b"replacement folder new file\n",
    )
    .unwrap();
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), done)
        .await
        .unwrap()
        .unwrap();
    let after = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    rig.runtime.shutdown().await.unwrap();
    assert_eq!(after, before);
    assert_eq!(
        fs::read(original.join("New.md")).unwrap(),
        b"original folder new file\n"
    );
    assert_eq!(
        fs::read(notes.join("Folder/New.md")).unwrap(),
        b"replacement folder new file\n"
    );
}

#[tokio::test]
async fn watcher_new_markdown_racing_api_create_does_not_duplicate_node() {
    use std::time::Duration;
    let (rig, folder) = RuntimeRig::new_with_registered_folder_watch().await;
    let handle = rig.runtime.handle();
    let before = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let (entered, release, done) = handle.test_pause_watch_lookup();
    let path = rig.dir.path().join("notes/Folder/New.md");
    fs::write(&path, b"transient external file\n").unwrap();
    tokio::time::timeout(Duration::from_secs(10), entered)
        .await
        .unwrap()
        .unwrap();
    fs::remove_file(&path).unwrap();
    let request = CreateNode {
        epoch: rig.paired.workspace_epoch,
        stream_id: rig.paired.device_stream_id.parse().unwrap(),
        operation_seq: 1,
        operation_id: op_id(),
        node_id: node_id(),
        parent_id: folder,
        name: "New.md".into(),
        kind: notryx_protocol::NodeKind::File,
        upload_id: None,
    };
    let receipt = handle
        .create_node(rig.paired.device_token.clone(), request.clone())
        .await
        .unwrap();
    assert!(matches!(&receipt.outcome, Outcome::Created(_)));
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), done)
        .await
        .unwrap()
        .unwrap();
    tokio::time::sleep(Duration::from_millis(700)).await;
    let after = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let page = handle
        .changes(rig.paired.device_token.clone(), before, Some(after))
        .await
        .unwrap();
    let receipt_after = handle
        .operation(rig.paired.device_token.clone(), request.operation_id)
        .await
        .unwrap();
    let bytes = handle
        .content(rig.paired.device_token.clone(), request.node_id, None)
        .await
        .unwrap()
        .1;
    rig.runtime.shutdown().await.unwrap();
    assert_eq!(after.get(), before.get() + 1);
    assert_eq!(page.nodes.len(), 2);
    assert_eq!(
        page.nodes
            .iter()
            .filter(|node| node.parent_id == Some(folder) && node.name == "New.md")
            .count(),
        1
    );
    assert_eq!(receipt_after, receipt);
    assert!(bytes.is_empty());
    assert!(fs::read(path).unwrap().is_empty());
}

#[tokio::test]
async fn watcher_disappearing_new_markdown_does_not_create_node() {
    use std::time::Duration;
    let (rig, folder) = RuntimeRig::new_with_registered_folder_watch().await;
    let handle = rig.runtime.handle();
    let before = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let parent = handle
        .node_state(rig.paired.device_token.clone(), folder)
        .await
        .unwrap();
    let (entered, release, done) = handle.test_pause_watch_lookup();
    let path = rig.dir.path().join("notes/Folder/New.md");
    fs::write(&path, b"transient\n").unwrap();
    tokio::time::timeout(Duration::from_secs(10), entered)
        .await
        .unwrap()
        .unwrap();
    fs::remove_file(&path).unwrap();
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), done)
        .await
        .unwrap()
        .unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;
    let after = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let parent_after = handle
        .node_state(rig.paired.device_token.clone(), folder)
        .await
        .unwrap();
    let degraded = handle.known_file_watch_degraded();
    rig.runtime.shutdown().await.unwrap();
    assert_eq!(after, before);
    assert_eq!(parent_after, parent);
    assert!(!degraded);
    assert!(!path.exists());
}

#[tokio::test]
async fn watcher_bounds_event_batches_and_shutdown_drains_after_burst() {
    use std::time::Duration;

    let rig = RuntimeRig::new_with_watch(true).await;
    let handle = rig.runtime.handle();
    let notes = rig.dir.path().join("notes");
    let state = rig.dir.path().join("state");
    let source = notes.join("Document.md");
    let barrier = handle.test_barrier().await;
    fs::write(&source, b"initial burst\n").unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while handle.pending_commands() == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("watcher did not enqueue its lookup");
    handle.test_reset_known_file_watch_max_batch();
    for index in 0..600 {
        fs::write(&source, format!("burst {index}\n")).unwrap();
    }
    barrier.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while handle.test_known_file_watch_max_batch() == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("watcher did not process the queued events");
    tokio::time::sleep(Duration::from_millis(200)).await;
    let largest_batch = handle.test_known_file_watch_max_batch();
    tokio::time::timeout(Duration::from_secs(10), rig.runtime.shutdown())
        .await
        .expect("watcher did not drain at shutdown")
        .unwrap();
    let mut reopened = Workspace::open(&notes, &state).await.unwrap();
    assert!(reopened.state.pending().await.unwrap().is_empty());
    assert_eq!(fs::read(source).unwrap(), b"burst 599\n");
    reopened.close().await.unwrap();
    assert!(
        largest_batch <= 128,
        "watcher read an unbounded event batch"
    );
}

#[tokio::test]
async fn watcher_registration_retries_transient_actor_queue_busy() {
    use std::time::Duration;

    let rig = RuntimeRig::new().await;
    let handle = rig.runtime.handle();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
    let registration_handle = handle.clone();
    let mut registration = tokio::spawn(async move {
        registration_handle
            .test_watch_registration_with_pause(entered_tx, resume_rx)
            .await
    });
    tokio::time::timeout(Duration::from_secs(10), entered_rx)
        .await
        .unwrap()
        .unwrap();
    let actor_release = handle.test_barrier().await;
    let mut requests = Vec::new();
    for _ in 0..32 {
        let request_handle = handle.clone();
        requests.push(tokio::spawn(async move { request_handle.health().await }));
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        while handle.pending_commands() < 32 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("actor queue did not fill");
    resume_tx.send(()).unwrap();
    let early = tokio::time::timeout(Duration::from_millis(200), &mut registration).await;
    actor_release.send(()).unwrap();
    for request in requests {
        request.await.unwrap().unwrap();
    }
    let result = match early {
        Ok(result) => result.unwrap(),
        Err(_) => tokio::time::timeout(Duration::from_secs(10), registration)
            .await
            .unwrap()
            .unwrap(),
    };
    rig.runtime.shutdown().await.unwrap();
    assert!(
        result.is_ok(),
        "transient actor queue pressure must not end watch registration"
    );
}

#[tokio::test]
async fn watcher_rejects_folder_replaced_during_registration() {
    use std::time::Duration;

    let rig = RuntimeRig::new().await;
    let handle = rig.runtime.handle();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
    let registration = tokio::spawn(async move {
        handle
            .test_watch_registration_with_pause(entered_tx, resume_rx)
            .await
    });
    tokio::time::timeout(Duration::from_secs(10), entered_rx)
        .await
        .unwrap()
        .unwrap();
    let notes = rig.dir.path().join("notes");
    let original = rig.dir.path().join("old-notes");
    fs::rename(&notes, &original).unwrap();
    fs::create_dir(&notes).unwrap();
    resume_tx.send(()).unwrap();
    let result = tokio::time::timeout(Duration::from_secs(10), registration)
        .await
        .unwrap()
        .unwrap();
    rig.runtime.shutdown().await.unwrap();
    assert!(result.is_err(), "replaced folder must not be accepted");
    assert_eq!(
        fs::read(original.join("Document.md")).unwrap(),
        b"original document\n"
    );
}

#[tokio::test]
async fn watcher_rejects_folder_replaced_after_lookup_before_file_observation() {
    check_watched_parent_replacement(false).await;
}

#[tokio::test]
async fn watcher_fallback_scan_does_not_adopt_replacement_folder() {
    check_watched_parent_replacement(true).await;
}

#[tokio::test]
async fn startup_scan_does_not_adopt_folder_replaced_after_watch_registration() {
    check_startup_watch_boundary(0).await;
}

#[tokio::test]
async fn startup_scan_skips_unverified_files_when_watcher_registration_fails() {
    check_startup_watch_boundary(1).await;
}

#[tokio::test]
async fn startup_scan_marks_watcher_ready_channel_disconnect_degraded() {
    check_startup_watch_boundary(2).await;
}

#[tokio::test]
async fn startup_scan_adopts_current_path_after_offline_folder_replacement() {
    check_startup_watch_boundary(3).await;
}

#[tokio::test]
async fn api_save_uses_current_path_after_offline_folder_replacement() {
    use crate::config::WorkspaceConfig;
    use crate::runtime::WorkspaceRuntime;

    let RuntimeRig {
        dir,
        runtime,
        document,
        paired,
        ..
    } = RuntimeRig::new().await;
    runtime.shutdown().await.unwrap();
    let notes = dir.path().join("notes");
    let state = dir.path().join("state");
    let mut workspace = Workspace::open(&notes, &state).await.unwrap();
    let root = workspace.state.root_id;
    let stream = workspace.register_stream().await.unwrap();
    let folder_id = node_id();
    assert!(matches!(
        workspace
            .create_node(CreateNode {
                epoch: workspace.epoch(),
                stream_id: stream,
                operation_seq: 1,
                operation_id: op_id(),
                node_id: folder_id,
                parent_id: root,
                name: "Folder".into(),
                kind: notryx_protocol::NodeKind::Folder,
                upload_id: None,
            })
            .await
            .unwrap()
            .outcome,
        Outcome::Created(_)
    ));
    assert!(matches!(
        workspace
            .move_node(MoveNode {
                epoch: workspace.epoch(),
                stream_id: stream,
                operation_seq: 2,
                operation_id: op_id(),
                node_id: document.node_id,
                expected_meta_rev: document.meta_rev,
                expected_parent_id: root,
                parent_id: folder_id,
                name: "Document.md".into(),
            })
            .await
            .unwrap()
            .outcome,
        Outcome::Moved(_)
    ));
    let before = workspace.state.head_seq().await.unwrap();
    workspace.close().await.unwrap();
    let old_folder = dir.path().join("original-folder");
    fs::rename(notes.join("Folder"), &old_folder).unwrap();
    fs::create_dir(notes.join("Folder")).unwrap();
    fs::write(notes.join("Folder/Document.md"), b"original document\n").unwrap();
    let mut runtime = WorkspaceRuntime::start(WorkspaceConfig {
        notes_root: notes.clone(),
        state_root: state.clone(),
        uploads: Default::default(),
    })
    .await
    .unwrap();
    runtime.test_wait_startup_scan().await;
    let handle = runtime.handle();
    let edit = b"new local edit\n";
    let request = SaveContent {
        epoch: paired.workspace_epoch,
        stream_id: paired.device_stream_id.parse().unwrap(),
        operation_seq: 1,
        operation_id: op_id(),
        node_id: document.node_id,
        expected_content_rev: document.content_rev.clone(),
        content_hash: digest(edit),
        bytes: edit.to_vec(),
    };
    let result = handle
        .save_content(paired.device_token.clone(), request.clone())
        .await;
    let receipt_present = handle
        .operation(paired.device_token.clone(), request.operation_id)
        .await
        .is_ok();
    let head = handle
        .server_info(paired.device_token)
        .await
        .unwrap()
        .head_seq;
    let original = fs::read(old_folder.join("Document.md")).unwrap();
    let replacement = fs::read(notes.join("Folder/Document.md")).unwrap();
    let old_object = fs::read(state.join("objects/sha256").join(&document.content_hash)).unwrap();
    let new_object_exists = state.join("objects/sha256").join(digest(edit)).exists();
    eprintln!(
        "offline folder save observation: accepted={}, receipt={}, head_changed={}, original_preserved={}, replacement_preserved={}, old_object_preserved={}, new_object_exists={}",
        result.is_ok(),
        receipt_present,
        head != before,
        original == b"original document\n",
        replacement == b"original document\n",
        old_object == b"original document\n",
        new_object_exists
    );
    runtime.shutdown().await.unwrap();
    // Offline folder replacements follow the current path by user-selected policy.
    assert!(matches!(result.unwrap().outcome, Outcome::Saved(_)));
    assert!(receipt_present);
    assert_eq!(head.get(), before.get() + 1);
    assert_eq!(original, b"original document\n");
    assert_eq!(replacement, edit);
    assert_eq!(old_object, b"original document\n");
    assert!(new_object_exists);
}

#[tokio::test]
async fn api_create_uses_current_path_after_offline_folder_replacement() {
    check_offline_structure_mutation("create").await;
}

#[tokio::test]
async fn api_move_uses_current_path_after_offline_folder_replacement() {
    check_offline_structure_mutation("move").await;
}

#[tokio::test]
async fn api_delete_uses_current_path_after_offline_folder_replacement() {
    check_offline_structure_mutation("delete").await;
}

#[tokio::test]
async fn api_restore_uses_current_path_after_offline_folder_replacement() {
    check_offline_structure_mutation("restore").await;
}

#[tokio::test]
async fn create_rejects_destination_parent_replaced_after_admission() {
    check_destination_replaced_after_admission("create", Phase::AfterAdmission, 0).await;
}

#[tokio::test]
async fn move_rejects_destination_parent_replaced_after_admission() {
    check_destination_replaced_after_admission("move", Phase::AfterAdmission, 0).await;
}

#[tokio::test]
async fn restore_rejects_destination_parent_replaced_after_admission() {
    check_destination_replaced_after_admission("restore", Phase::AfterAdmission, 0).await;
}

#[tokio::test]
async fn create_rejects_destination_parent_replaced_before_storage_open() {
    check_destination_replaced_after_admission("create", Phase::BeforeDestinationOpen, 0).await;
}

#[tokio::test]
async fn move_rejects_destination_parent_replaced_before_storage_open() {
    check_destination_replaced_after_admission("move", Phase::BeforeDestinationOpen, 0).await;
}

#[tokio::test]
async fn restore_rejects_destination_parent_replaced_before_storage_open() {
    check_destination_replaced_after_admission("restore", Phase::BeforeDestinationOpen, 0).await;
}

#[tokio::test]
async fn create_parent_mismatch_recovery_quarantines_pending_intent() {
    check_destination_replaced_after_admission("create", Phase::AfterAdmission, 1).await;
}

#[tokio::test]
async fn move_parent_mismatch_recovery_quarantines_pending_intent() {
    check_destination_replaced_after_admission("move", Phase::AfterAdmission, 1).await;
}

#[tokio::test]
async fn restore_parent_mismatch_recovery_quarantines_pending_intent() {
    check_destination_replaced_after_admission("restore", Phase::AfterAdmission, 1).await;
}

#[tokio::test]
async fn legacy_create_without_parent_evidence_is_quarantined() {
    check_destination_replaced_after_admission("create", Phase::AfterAdmission, 2).await;
}

#[tokio::test]
async fn legacy_move_without_parent_evidence_is_quarantined() {
    check_destination_replaced_after_admission("move", Phase::AfterAdmission, 2).await;
}

#[tokio::test]
async fn legacy_restore_without_parent_evidence_is_quarantined() {
    check_destination_replaced_after_admission("restore", Phase::AfterAdmission, 2).await;
}

async fn check_destination_replaced_after_admission(
    kind: &str,
    target_phase: Phase,
    recovery_case: u8,
) {
    let mut rig = Rig::new().await;
    let root = rig.workspace.state.root_id;
    let folder_id = node_id();
    let folder_request = CreateNode {
        epoch: rig.workspace.epoch(),
        stream_id: rig.stream,
        operation_seq: 1,
        operation_id: op_id(),
        node_id: folder_id,
        parent_id: root,
        name: "Folder".into(),
        kind: notryx_protocol::NodeKind::Folder,
        upload_id: None,
    };
    let existing_receipt = rig
        .workspace
        .create_node(folder_request.clone())
        .await
        .unwrap();
    assert!(matches!(existing_receipt.outcome, Outcome::Created(_)));
    let tombstone = if kind == "restore" {
        let node = NodeSnapshot::from(&rig.initial);
        let Outcome::Deleted(deleted) = rig
            .workspace
            .delete_node(DeleteNode {
                epoch: rig.workspace.epoch(),
                stream_id: rig.stream,
                operation_seq: 2,
                operation_id: op_id(),
                node_id: node.node_id,
                expected_node_etag: node.etag().unwrap(),
            })
            .await
            .unwrap()
            .outcome
        else {
            panic!("delete fixture")
        };
        Some(deleted)
    } else {
        None
    };
    let before = rig.workspace.state.head_seq().await.unwrap();
    let source = rig.dir.path().join("notes/Document.md");
    let old_folder = rig.dir.path().join("old-folder");
    let folder_path = rig.dir.path().join("notes/Folder");
    fs::write(
        folder_path.join("evidence.txt"),
        b"original folder evidence\n",
    )
    .unwrap();
    let hook_old = old_folder.clone();
    let hook_folder = folder_path.clone();
    rig.workspace.hook = Some(Box::new(move |phase| {
        if phase == target_phase {
            fs::rename(&hook_folder, &hook_old).unwrap();
            fs::create_dir(&hook_folder).unwrap();
            fs::write(
                hook_folder.join("evidence.txt"),
                b"replacement folder evidence\n",
            )
            .unwrap();
        }
        Ok(())
    }));
    let sequence = if kind == "restore" { 3 } else { 2 };
    let operation_id = op_id();
    let result = match kind {
        "create" => {
            rig.workspace
                .create_node(CreateNode {
                    epoch: rig.workspace.epoch(),
                    stream_id: rig.stream,
                    operation_seq: sequence,
                    operation_id,
                    node_id: node_id(),
                    parent_id: folder_id,
                    name: "New.md".into(),
                    kind: notryx_protocol::NodeKind::File,
                    upload_id: None,
                })
                .await
        }
        "move" => {
            rig.workspace
                .move_node(MoveNode {
                    epoch: rig.workspace.epoch(),
                    stream_id: rig.stream,
                    operation_seq: sequence,
                    operation_id,
                    node_id: rig.initial.node_id,
                    expected_meta_rev: rig.initial.meta_rev.clone(),
                    expected_parent_id: root,
                    parent_id: folder_id,
                    name: "Moved.md".into(),
                })
                .await
        }
        "restore" => {
            rig.workspace
                .restore_node(RestoreNode {
                    epoch: rig.workspace.epoch(),
                    stream_id: rig.stream,
                    operation_seq: sequence,
                    operation_id,
                    node_id: rig.initial.node_id,
                    expected_meta_rev: tombstone.unwrap().meta_rev,
                    parent_id: folder_id,
                    name: "Restored.md".into(),
                })
                .await
        }
        _ => panic!("unknown operation"),
    };
    let after = rig.workspace.state.head_seq().await.unwrap();
    let pending = rig.workspace.state.pending().await.unwrap();
    let replacement_target = folder_path.join(match kind {
        "create" => "New.md",
        "move" => "Moved.md",
        _ => "Restored.md",
    });
    let source_bytes = fs::read(&source).ok();
    let old_receipt = rig
        .workspace
        .create_node(folder_request.clone())
        .await
        .unwrap();
    eprintln!(
        "destination replacement {kind}: accepted={}, head_changed={}, pending={}, replacement_target={}, source_present={}",
        result.is_ok(),
        after != before,
        pending.len(),
        replacement_target.exists(),
        source_bytes.is_some()
    );
    assert!(
        result.is_err(),
        "changed destination parent must not receive the mutation"
    );
    assert_eq!(after, before);
    assert_eq!(pending.len(), 1);
    assert_eq!(old_receipt, existing_receipt);
    assert!(!replacement_target.exists());
    assert_eq!(
        fs::read(old_folder.join("evidence.txt")).unwrap(),
        b"original folder evidence\n"
    );
    assert_eq!(
        fs::read(folder_path.join("evidence.txt")).unwrap(),
        b"replacement folder evidence\n"
    );
    if kind != "restore" {
        assert_eq!(
            source_bytes.unwrap(),
            b"\xef\xbb\xbf# Original\r\n\r\n<!-- untouched -->\r\n"
        );
    }
    assert_eq!(
        fs::read(
            rig.dir
                .path()
                .join("state/objects/sha256")
                .join(&rig.initial.content_hash)
        )
        .unwrap(),
        b"\xef\xbb\xbf# Original\r\n\r\n<!-- untouched -->\r\n"
    );
    if recovery_case != 0 {
        let intent_json: String =
            sqlx::query_scalar("SELECT intent FROM write_intents WHERE operation_id=?")
                .bind(operation_id.to_string())
                .fetch_one(&mut rig.workspace.state.connection)
                .await
                .unwrap();
        assert!(!legacy_intent_parser_accepts(kind, &intent_json));
        if recovery_case == 2 {
            fs::rename(&folder_path, rig.dir.path().join("replacement-folder")).unwrap();
            fs::rename(&old_folder, &folder_path).unwrap();
            sqlx::query("UPDATE write_intents SET intent=json_remove(intent, '$.destination_parent') WHERE operation_id=?")
                .bind(operation_id.to_string()).execute(&mut rig.workspace.state.connection).await.unwrap();
            let legacy_json: String =
                sqlx::query_scalar("SELECT intent FROM write_intents WHERE operation_id=?")
                    .bind(operation_id.to_string())
                    .fetch_one(&mut rig.workspace.state.connection)
                    .await
                    .unwrap();
            assert!(legacy_intent_parser_accepts(kind, &legacy_json));
        }
        rig.workspace.hook = None;
        rig.workspace.close().await.unwrap();
        let mut reopened =
            Workspace::open(&rig.dir.path().join("notes"), &rig.dir.path().join("state"))
                .await
                .unwrap();
        assert!(!reopened.ready());
        assert_eq!(reopened.state.head_seq().await.unwrap(), before);
        assert_eq!(reopened.state.pending().await.unwrap().len(), 1);
        assert!(reopened.state.is_quarantined(operation_id).await.unwrap());
        assert_eq!(
            reopened.create_node(folder_request).await.unwrap(),
            existing_receipt
        );
        let recovery_json: String =
            sqlx::query_scalar("SELECT intent FROM recovery_cases WHERE operation_id=?")
                .bind(operation_id.to_string())
                .fetch_one(&mut reopened.state.connection)
                .await
                .unwrap();
        assert_eq!(
            legacy_intent_parser_accepts(kind, &recovery_json),
            recovery_case == 2
        );
        assert!(!replacement_target.exists());
        assert_eq!(
            fs::read(
                rig.dir
                    .path()
                    .join("state/objects/sha256")
                    .join(&rig.initial.content_hash)
            )
            .unwrap(),
            b"\xef\xbb\xbf# Original\r\n\r\n<!-- untouched -->\r\n"
        );
        reopened.close().await.unwrap();
    }
}

fn legacy_intent_parser_accepts(kind: &str, input: &str) -> bool {
    // Mirrors the strict intent fields in the previous committed version, not an old binary.
    #[allow(dead_code)]
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct LegacyCreate {
        operation_id: serde_json::Value,
        parent_before: serde_json::Value,
        created: serde_json::Value,
        path: serde_json::Value,
        #[serde(default)]
        upload_id: serde_json::Value,
    }
    #[allow(dead_code)]
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct LegacyMove {
        operation_id: serde_json::Value,
        before: serde_json::Value,
        proposed: serde_json::Value,
        parents_before: serde_json::Value,
        old_path: serde_json::Value,
        new_path: serde_json::Value,
        source_stamp: serde_json::Value,
        #[serde(default)]
        observed_hashes: serde_json::Value,
    }
    #[allow(dead_code)]
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct LegacyLifecycle {
        operation_id: serde_json::Value,
        before: serde_json::Value,
        proposed: serde_json::Value,
        parent_before: serde_json::Value,
        path: serde_json::Value,
        action: serde_json::Value,
    }
    match kind {
        "create" => serde_json::from_str::<LegacyCreate>(input).is_ok(),
        "move" => serde_json::from_str::<LegacyMove>(input).is_ok(),
        "restore" => serde_json::from_str::<LegacyLifecycle>(input).is_ok(),
        _ => false,
    }
}

async fn check_offline_structure_mutation(kind: &str) {
    use crate::config::WorkspaceConfig;
    use crate::runtime::WorkspaceRuntime;
    let RuntimeRig {
        dir,
        runtime,
        document,
        other,
        paired,
        ..
    } = RuntimeRig::new().await;
    let handle = runtime.handle();
    let prior = SaveContent {
        epoch: paired.workspace_epoch,
        stream_id: paired.device_stream_id.parse().unwrap(),
        operation_seq: 1,
        operation_id: op_id(),
        node_id: other.node_id,
        expected_content_rev: other.content_rev,
        content_hash: digest(b"prior saved\n"),
        bytes: b"prior saved\n".to_vec(),
    };
    let old_receipt = handle
        .save_content(paired.device_token.clone(), prior.clone())
        .await
        .unwrap();
    runtime.shutdown().await.unwrap();
    let notes = dir.path().join("notes");
    let state = dir.path().join("state");
    let mut workspace = Workspace::open(&notes, &state).await.unwrap();
    let root = workspace.state.root_id;
    let stream = workspace.register_stream().await.unwrap();
    let folder = node_id();
    assert!(matches!(
        workspace
            .create_node(CreateNode {
                epoch: workspace.epoch(),
                stream_id: stream,
                operation_seq: 1,
                operation_id: op_id(),
                node_id: folder,
                parent_id: root,
                name: "Folder".into(),
                kind: notryx_protocol::NodeKind::Folder,
                upload_id: None,
            })
            .await
            .unwrap()
            .outcome,
        Outcome::Created(_)
    ));
    let Outcome::Moved(moved) = workspace
        .move_node(MoveNode {
            epoch: workspace.epoch(),
            stream_id: stream,
            operation_seq: 2,
            operation_id: op_id(),
            node_id: document.node_id,
            expected_meta_rev: document.meta_rev,
            expected_parent_id: root,
            parent_id: folder,
            name: "Document.md".into(),
        })
        .await
        .unwrap()
        .outcome
    else {
        panic!("move fixture")
    };
    let other_node = NodeSnapshot::from(
        &workspace
            .state
            .document(other.node_id)
            .await
            .unwrap()
            .unwrap(),
    );
    let Outcome::Deleted(deleted) = workspace
        .delete_node(DeleteNode {
            epoch: workspace.epoch(),
            stream_id: stream,
            operation_seq: 3,
            operation_id: op_id(),
            node_id: other.node_id,
            expected_node_etag: other_node.etag().unwrap(),
        })
        .await
        .unwrap()
        .outcome
    else {
        panic!("delete fixture")
    };
    let before = workspace.state.head_seq().await.unwrap();
    workspace.close().await.unwrap();
    let original_folder = dir.path().join("original-folder");
    fs::rename(notes.join("Folder"), &original_folder).unwrap();
    fs::create_dir(notes.join("Folder")).unwrap();
    fs::write(notes.join("Folder/Document.md"), b"original document\n").unwrap();
    let mut runtime = WorkspaceRuntime::start(WorkspaceConfig {
        notes_root: notes.clone(),
        state_root: state.clone(),
        uploads: Default::default(),
    })
    .await
    .unwrap();
    runtime.test_wait_startup_scan().await;
    let handle = runtime.handle();
    let operation_id = op_id();
    let stream_id = paired.device_stream_id.parse().unwrap();
    let result = match kind {
        "create" => {
            handle
                .create_node(
                    paired.device_token.clone(),
                    CreateNode {
                        epoch: paired.workspace_epoch,
                        stream_id,
                        operation_seq: 2,
                        operation_id,
                        node_id: node_id(),
                        parent_id: folder,
                        name: "Inserted".into(),
                        kind: notryx_protocol::NodeKind::Folder,
                        upload_id: None,
                    },
                )
                .await
        }
        "move" => {
            handle
                .move_node(
                    paired.device_token.clone(),
                    MoveNode {
                        epoch: paired.workspace_epoch,
                        stream_id,
                        operation_seq: 2,
                        operation_id,
                        node_id: moved.node_id,
                        expected_meta_rev: moved.meta_rev.clone(),
                        expected_parent_id: folder,
                        parent_id: root,
                        name: "Moved.md".into(),
                    },
                )
                .await
        }
        "delete" => {
            handle
                .delete_node(
                    paired.device_token.clone(),
                    DeleteNode {
                        epoch: paired.workspace_epoch,
                        stream_id,
                        operation_seq: 2,
                        operation_id,
                        node_id: moved.node_id,
                        expected_node_etag: moved.etag().unwrap(),
                    },
                )
                .await
        }
        "restore" => {
            handle
                .restore_node(
                    paired.device_token.clone(),
                    RestoreNode {
                        epoch: paired.workspace_epoch,
                        stream_id,
                        operation_seq: 2,
                        operation_id,
                        node_id: deleted.node_id,
                        expected_meta_rev: deleted.meta_rev.clone(),
                        parent_id: folder,
                        name: "Other.md".into(),
                    },
                )
                .await
        }
        _ => panic!("unknown operation"),
    };
    let receipt = handle
        .operation(paired.device_token.clone(), prior.operation_id)
        .await
        .unwrap();
    let new_receipt = handle
        .operation(paired.device_token.clone(), operation_id)
        .await
        .is_ok();
    let head = handle
        .server_info(paired.device_token)
        .await
        .unwrap()
        .head_seq;
    let original = fs::read(original_folder.join("Document.md")).ok();
    let replacement = fs::read(notes.join("Folder/Document.md")).ok();
    let old_object = fs::read(state.join("objects/sha256").join(&document.content_hash)).ok();
    eprintln!(
        "offline structure {kind}: accepted={}, new_receipt={}, head_changed={}, original_preserved={}, replacement_preserved={}, object_preserved={}",
        result.is_ok(),
        new_receipt,
        head != before,
        original.as_deref() == Some(b"original document\n"),
        replacement.as_deref() == Some(b"original document\n"),
        old_object.as_deref() == Some(b"original document\n")
    );
    runtime.shutdown().await.unwrap();
    // Offline folder replacements follow the current path by user-selected policy.
    let outcome = result.unwrap().outcome;
    assert!(matches!(
        (kind, outcome),
        ("create", Outcome::Created(_))
            | ("move", Outcome::Moved(_))
            | ("delete", Outcome::Deleted(_))
            | ("restore", Outcome::Restored(_))
    ));
    assert!(new_receipt);
    assert_eq!(receipt, old_receipt);
    assert_eq!(head.get(), before.get() + 1);
    assert_eq!(original.as_deref(), Some(b"original document\n".as_slice()));
    if kind == "move" || kind == "delete" {
        assert!(replacement.is_none());
    } else {
        assert_eq!(
            replacement.as_deref(),
            Some(b"original document\n".as_slice())
        );
    }
    if kind == "create" {
        assert!(notes.join("Folder/Inserted").is_dir());
    }
    if kind == "move" {
        assert_eq!(
            fs::read(notes.join("Moved.md")).unwrap(),
            b"original document\n"
        );
    }
    if kind == "restore" {
        assert_eq!(
            fs::read(notes.join("Folder/Other.md")).unwrap(),
            b"prior saved\n"
        );
    }
    assert_eq!(
        old_object.as_deref(),
        Some(b"original document\n".as_slice())
    );
}

async fn check_startup_watch_boundary(mode: u8) {
    use crate::config::WorkspaceConfig;
    use crate::runtime::WorkspaceRuntime;
    use std::time::Duration;

    let RuntimeRig {
        dir,
        runtime,
        document,
        paired,
        ..
    } = RuntimeRig::new().await;
    runtime.shutdown().await.unwrap();
    let notes = dir.path().join("notes");
    let state = dir.path().join("state");
    let mut workspace = Workspace::open(&notes, &state).await.unwrap();
    let root = workspace.state.root_id;
    let stream = workspace.register_stream().await.unwrap();
    let folder_id = node_id();
    let folder = CreateNode {
        epoch: workspace.epoch(),
        stream_id: stream,
        operation_seq: 1,
        operation_id: op_id(),
        node_id: folder_id,
        parent_id: root,
        name: "Folder".into(),
        kind: notryx_protocol::NodeKind::Folder,
        upload_id: None,
    };
    assert!(matches!(
        workspace.create_node(folder).await.unwrap().outcome,
        Outcome::Created(_)
    ));
    let moved = MoveNode {
        epoch: workspace.epoch(),
        stream_id: stream,
        operation_seq: 2,
        operation_id: op_id(),
        node_id: document.node_id,
        expected_meta_rev: document.meta_rev,
        expected_parent_id: root,
        parent_id: folder_id,
        name: "Document.md".into(),
    };
    assert!(matches!(
        workspace.move_node(moved).await.unwrap().outcome,
        Outcome::Moved(_)
    ));
    let before = workspace.state.head_seq().await.unwrap();
    workspace.close().await.unwrap();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    if mode == 1 || mode == 2 {
        fs::write(notes.join("Folder/Document.md"), b"unverified change\n").unwrap();
    }
    let original_folder = dir.path().join("original-folder");
    if mode == 3 {
        fs::rename(notes.join("Folder"), &original_folder).unwrap();
        fs::create_dir(notes.join("Folder")).unwrap();
        fs::write(notes.join("Folder/Document.md"), b"replacement folder\n").unwrap();
    }
    let mut runtime = WorkspaceRuntime::test_start_with_watch_control(
        WorkspaceConfig {
            notes_root: notes.clone(),
            state_root: state,
            uploads: Default::default(),
        },
        mode == 1,
        mode == 2,
        Some((entered_tx, release_rx)),
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(10), entered_rx)
        .await
        .unwrap()
        .unwrap();
    if mode == 0 {
        fs::rename(notes.join("Folder"), &original_folder).unwrap();
        fs::create_dir(notes.join("Folder")).unwrap();
        fs::write(notes.join("Folder/Document.md"), b"replacement folder\n").unwrap();
    }
    release_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), runtime.test_wait_startup_scan())
        .await
        .unwrap();
    let handle = runtime.handle();
    let after = handle
        .server_info(paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let (_, bytes) = handle
        .content(paired.device_token, document.node_id, None)
        .await
        .unwrap();
    let incomplete = handle.known_file_scan_incomplete();
    let degraded = handle.known_file_watch_degraded();
    runtime.shutdown().await.unwrap();
    if mode == 3 {
        // The server cannot distinguish offline folder replacement from a same-path edit.
        assert_eq!(after.get(), before.get() + 1);
        assert_eq!(bytes, b"replacement folder\n");
        assert!(!incomplete);
    } else {
        assert_eq!(
            after, before,
            "unverified folder must not advance the existing node"
        );
        assert_eq!(bytes, b"original document\n");
        assert!(incomplete, "unverified startup scan must be incomplete");
    }
    if mode == 1 || mode == 2 {
        assert!(degraded);
    }
    if mode == 0 || mode == 3 {
        assert_eq!(
            fs::read(original_folder.join("Document.md")).unwrap(),
            b"original document\n"
        );
        assert_eq!(
            fs::read(notes.join("Folder/Document.md")).unwrap(),
            b"replacement folder\n"
        );
    }
}

async fn check_watched_parent_replacement(run_fallback: bool) {
    use crate::config::WorkspaceConfig;
    use crate::runtime::WorkspaceRuntime;
    use std::time::Duration;

    let RuntimeRig {
        dir,
        runtime,
        document,
        paired,
        ..
    } = RuntimeRig::new().await;
    runtime.shutdown().await.unwrap();
    let notes = dir.path().join("notes");
    let state = dir.path().join("state");
    let mut workspace = Workspace::open(&notes, &state).await.unwrap();
    let root = workspace.state.root_id;
    let stream = workspace.register_stream().await.unwrap();
    let folder_id = node_id();
    let folder = CreateNode {
        epoch: workspace.epoch(),
        stream_id: stream,
        operation_seq: 1,
        operation_id: op_id(),
        node_id: folder_id,
        parent_id: root,
        name: "Folder".into(),
        kind: notryx_protocol::NodeKind::Folder,
        upload_id: None,
    };
    assert!(matches!(
        workspace.create_node(folder).await.unwrap().outcome,
        Outcome::Created(_)
    ));
    let moved = MoveNode {
        epoch: workspace.epoch(),
        stream_id: stream,
        operation_seq: 2,
        operation_id: op_id(),
        node_id: document.node_id,
        expected_meta_rev: document.meta_rev,
        expected_parent_id: root,
        parent_id: folder_id,
        name: "Document.md".into(),
    };
    assert!(matches!(
        workspace.move_node(moved).await.unwrap().outcome,
        Outcome::Moved(_)
    ));
    workspace.close().await.unwrap();
    let mut runtime = WorkspaceRuntime::start(WorkspaceConfig {
        notes_root: notes.clone(),
        state_root: state,
        uploads: Default::default(),
    })
    .await
    .unwrap();
    runtime.test_wait_startup_scan().await;
    let handle = runtime.handle();
    let before = handle
        .server_info(paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let scan_events_before = handle.test_known_file_scan_events().len();
    let mut scan_lock = Some(handle.test_lock_known_file_scan().await);
    let (entered, release, done) = handle.test_pause_watch_lookup();
    fs::write(notes.join("Folder/Document.md"), b"original folder edit\n").unwrap();
    tokio::time::timeout(Duration::from_secs(10), entered)
        .await
        .unwrap()
        .unwrap();
    let old_folder = dir.path().join("original-folder");
    fs::rename(notes.join("Folder"), &old_folder).unwrap();
    fs::create_dir(notes.join("Folder")).unwrap();
    fs::write(
        notes.join("Folder/Document.md"),
        b"replacement folder bytes\n",
    )
    .unwrap();
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), done)
        .await
        .unwrap()
        .unwrap();
    if run_fallback {
        tokio::time::timeout(Duration::from_secs(10), async {
            while !handle.test_known_file_watch_scan_waiting() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("watcher did not request fallback scan");
        drop(scan_lock.take());
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let events = handle.test_known_file_scan_events();
                if events.iter().skip(scan_events_before).any(|event| {
                    matches!(
                        *event,
                        "known_file_scan_complete" | "known_file_scan_incomplete"
                    )
                }) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("fallback scan did not finish");
    }
    let after = handle
        .server_info(paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let (_, bytes) = handle
        .content(paired.device_token.clone(), document.node_id, None)
        .await
        .unwrap();
    drop(scan_lock.take());
    runtime.shutdown().await.unwrap();
    assert_eq!(
        after, before,
        "replacement folder must not advance the existing node"
    );
    assert_eq!(bytes, b"original document\n");
    assert_eq!(
        fs::read(old_folder.join("Document.md")).unwrap(),
        b"original folder edit\n"
    );
}

#[tokio::test]
async fn watcher_drains_inotify_while_a_rescan_waits() {
    use std::time::Duration;

    let rig = RuntimeRig::new_with_watch(true).await;
    let handle = rig.runtime.handle();
    let scan_lock = handle.test_lock_known_file_scan().await;
    fs::create_dir(rig.dir.path().join("notes/external-folder")).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !handle.test_known_file_watch_scan_waiting() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("watcher did not request rescan");
    handle.test_reset_known_file_watch_max_batch();
    fs::write(rig.dir.path().join("notes/Document.md"), b"during scan\n").unwrap();
    let drained = tokio::time::timeout(Duration::from_secs(2), async {
        while handle.test_known_file_watch_max_batch() == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok();
    drop(scan_lock);
    tokio::time::timeout(Duration::from_secs(10), rig.runtime.shutdown())
        .await
        .unwrap()
        .unwrap();
    assert!(
        drained,
        "watcher did not drain inotify while rescan was waiting"
    );
}

#[tokio::test]
async fn periodic_scan_finds_existing_file_when_watcher_events_are_missed() {
    use crate::config::WorkspaceConfig;
    use crate::runtime::WorkspaceRuntime;
    use std::time::Duration;

    let RuntimeRig {
        dir,
        runtime,
        document,
        paired,
        ..
    } = RuntimeRig::new().await;
    runtime.shutdown().await.unwrap();
    let notes = dir.path().join("notes");
    let state = dir.path().join("state");
    let mut runtime = WorkspaceRuntime::test_start_with_sweep_interval(
        WorkspaceConfig {
            notes_root: notes.clone(),
            state_root: state.clone(),
            uploads: Default::default(),
        },
        Duration::from_millis(400),
    )
    .await
    .unwrap();
    runtime.test_wait_startup_scan().await;
    let handle = runtime.handle();
    let before = handle
        .server_info(paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let original_object = state.join("objects/sha256").join(&document.content_hash);
    handle.test_suppress_watch_name("Document.md");
    let external = b"change without a delivered watcher event\n";
    fs::write(notes.join("Document.md"), external).unwrap();
    let result = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let head = handle
                .server_info(paired.device_token.clone())
                .await
                .unwrap()
                .head_seq;
            if head != before {
                break head;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    let found = if result.is_ok() {
        Some(
            handle
                .content(paired.device_token.clone(), document.node_id, None)
                .await
                .unwrap(),
        )
    } else {
        None
    };
    runtime.shutdown().await.unwrap();
    let head = result.expect("periodic scan did not find the missed event");
    let (updated, bytes) = found.unwrap();
    assert_eq!(head.get(), before.get() + 1);
    assert_eq!(updated.node_id, document.node_id);
    assert_eq!(updated.meta_rev, document.meta_rev);
    assert_eq!(bytes, external);
    assert_eq!(fs::read(original_object).unwrap(), b"original document\n");
    assert_eq!(fs::read(notes.join("Document.md")).unwrap(), external);
}

#[tokio::test]
async fn watcher_retries_rescan_after_task_panics() {
    use std::time::Duration;

    let rig = RuntimeRig::new_with_watch(true).await;
    let handle = rig.runtime.handle();
    let completed_before = handle
        .test_known_file_scan_events()
        .iter()
        .filter(|event| **event == "known_file_scan_complete")
        .count();
    handle.test_panic_next_watch_scan();
    fs::create_dir(rig.dir.path().join("notes/external-folder")).unwrap();
    let retried = tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            let completed = handle
                .test_known_file_scan_events()
                .iter()
                .filter(|event| **event == "known_file_scan_complete")
                .count();
            if completed > completed_before {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok();
    rig.runtime.shutdown().await.unwrap();
    assert!(retried, "rescan must be retried after a task panic");
    assert_eq!(
        fs::read(rig.dir.path().join("notes/Document.md")).unwrap(),
        b"original document\n"
    );
}

#[tokio::test]
async fn watcher_drains_inotify_while_single_file_lookup_waits() {
    use std::time::Duration;

    let rig = RuntimeRig::new_with_watch(true).await;
    let handle = rig.runtime.handle();
    let release = handle.test_barrier().await;
    fs::write(rig.dir.path().join("notes/Document.md"), b"first change\n").unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while handle.pending_commands() == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("watcher did not submit first lookup");
    handle.test_reset_known_file_watch_max_batch();
    fs::write(rig.dir.path().join("notes/Other.md"), b"second change\n").unwrap();
    let drained = tokio::time::timeout(Duration::from_secs(2), async {
        while handle.test_known_file_watch_max_batch() == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok();
    release.send(()).unwrap();
    rig.runtime.shutdown().await.unwrap();
    assert!(
        drained,
        "watcher did not drain inotify while a lookup was waiting"
    );
}

#[tokio::test]
async fn watcher_marks_unexpected_file_job_closed_as_degraded() {
    use std::time::Duration;
    let rig = RuntimeRig::new_with_watch(true).await;
    let handle = rig.runtime.handle();
    handle.test_watch_job_fault(1);
    fs::write(
        rig.dir.path().join("notes/Document.md"),
        b"unexpected closed\n",
    )
    .unwrap();
    let degraded = tokio::time::timeout(Duration::from_secs(3), async {
        while !handle.known_file_watch_degraded() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok();
    rig.runtime.shutdown().await.unwrap();
    assert!(degraded, "unexpected file job closure must be visible");
}

#[tokio::test]
async fn watcher_pending_limit_includes_retrying_file_job() {
    use std::time::Duration;
    let rig = RuntimeRig::new_with_watch(true).await;
    let handle = rig.runtime.handle();
    let release = handle.test_barrier().await;
    handle.test_watch_job_fault(2);
    fs::write(
        rig.dir.path().join("notes/Document.md"),
        b"blocked lookup\n",
    )
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while handle.pending_commands() == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("first lookup did not reach actor");
    for index in 0..256 {
        fs::write(
            rig.dir
                .path()
                .join("notes")
                .join(format!("pending-{index:03}.md")),
            b"synthetic\n",
        )
        .unwrap();
    }
    tokio::time::timeout(Duration::from_secs(10), async {
        while handle.test_watch_max_pending() < 256 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("watcher did not fill pending queue");
    release.send(()).unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let maximum = handle.test_watch_max_pending();
    rig.runtime.shutdown().await.unwrap();
    assert!(maximum <= 256, "pending exceeded its limit: {maximum}");
}

#[tokio::test]
async fn watcher_repeated_scan_panics_leave_scan_incomplete() {
    use std::time::Duration;
    let rig = RuntimeRig::new_with_watch(true).await;
    let handle = rig.runtime.handle();
    assert!(!handle.known_file_scan_incomplete());
    handle.test_panic_watch_scans(3);
    fs::create_dir(rig.dir.path().join("notes/external-folder")).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while handle.test_watch_scan_panics_remaining() != 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("scan task was not retried");
    tokio::time::sleep(Duration::from_millis(200)).await;
    let incomplete = handle.known_file_scan_incomplete();
    let degraded = handle.known_file_watch_degraded();
    rig.runtime.shutdown().await.unwrap();
    assert!(degraded);
    assert!(incomplete, "failed rescans must remain incomplete");
}

#[test]
fn watcher_registers_many_folders_with_low_process_fd_limit() {
    use std::io::{Read, Seek, SeekFrom};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let mut diagnostics = tempfile::tempfile().unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "tests::reconcile::known_file_watch_low_fd_limit_worker",
            "--ignored",
            "--nocapture",
        ])
        .env("NOTRYX_TEST_WATCH_LOW_FD", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(diagnostics.try_clone().unwrap()))
        .spawn()
        .unwrap();
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if started.elapsed() >= Duration::from_secs(40) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("low-fd watcher subprocess timed out");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    if !status.success() {
        diagnostics.seek(SeekFrom::Start(0)).unwrap();
        let mut output = String::new();
        diagnostics.read_to_string(&mut output).unwrap();
        panic!("low-fd watcher subprocess failed: {status}\n{output}");
    }
}

#[tokio::test]
#[ignore = "invoked by watcher_registers_many_folders_with_low_process_fd_limit"]
async fn known_file_watch_low_fd_limit_worker() {
    use crate::config::WorkspaceConfig;
    use crate::runtime::WorkspaceRuntime;
    use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
    use std::time::Duration;

    assert_eq!(
        std::env::var("NOTRYX_TEST_WATCH_LOW_FD").as_deref(),
        Ok("1")
    );
    let RuntimeRig {
        dir,
        runtime,
        document,
        other,
        paired,
        ..
    } = RuntimeRig::new().await;
    eprintln!("low-fd stage: create synthetic folders");
    runtime.shutdown().await.unwrap();
    let notes = dir.path().join("notes");
    let state = dir.path().join("state");
    let mut workspace = Workspace::open(&notes, &state).await.unwrap();
    let root = workspace.state.root_id;
    let stream = workspace.register_stream().await.unwrap();
    let mut last_folder = None;
    for index in 0..70u64 {
        let folder_id = node_id();
        let folder_name = format!("Folder-{index:03}");
        if last_folder
            .as_ref()
            .is_none_or(|(id, _): &(NodeId, String)| folder_id.to_string() > id.to_string())
        {
            last_folder = Some((folder_id, folder_name.clone()));
        }
        let request = CreateNode {
            epoch: workspace.epoch(),
            stream_id: stream,
            operation_seq: index + 1,
            operation_id: op_id(),
            node_id: folder_id,
            parent_id: root,
            name: folder_name,
            kind: notryx_protocol::NodeKind::Folder,
            upload_id: None,
        };
        assert!(matches!(
            workspace.create_node(request).await.unwrap().outcome,
            Outcome::Created(_)
        ));
    }
    let (last_folder_id, last_folder_name) = last_folder.unwrap();
    let moved = MoveNode {
        epoch: workspace.epoch(),
        stream_id: stream,
        operation_seq: 71,
        operation_id: op_id(),
        node_id: other.node_id,
        expected_meta_rev: other.meta_rev,
        expected_parent_id: root,
        parent_id: last_folder_id,
        name: "Other.md".into(),
    };
    assert!(matches!(
        workspace.move_node(moved).await.unwrap().outcome,
        Outcome::Moved(_)
    ));
    workspace.close().await.unwrap();
    eprintln!("low-fd stage: set child process limit");
    let original_limit = getrlimit(Resource::Nofile);
    assert!(original_limit.current.is_none_or(|limit| limit >= 48));
    setrlimit(
        Resource::Nofile,
        Rlimit {
            current: Some(48),
            maximum: original_limit.maximum,
        },
    )
    .unwrap();
    eprintln!("low-fd stage: start runtime and register watches");
    let mut runtime = WorkspaceRuntime::start(WorkspaceConfig {
        notes_root: notes.clone(),
        state_root: state,
        uploads: Default::default(),
    })
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(20), runtime.test_wait_startup_scan())
        .await
        .unwrap();
    let handle = runtime.handle();
    assert!(
        !handle.known_file_watch_degraded(),
        "registered folders should not exhaust process FDs"
    );
    eprintln!("low-fd stage: detect external write");
    let before = handle
        .server_info(paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let (_, bytes) = handle
        .content(paired.device_token.clone(), document.node_id, None)
        .await
        .unwrap();
    assert_eq!(bytes, b"original document\n");
    let source = notes.join("Document.md");
    let external = b"change under a low fd limit\n";
    fs::write(&source, external).unwrap();
    let head = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let current = handle
                .server_info(paired.device_token.clone())
                .await
                .unwrap()
                .head_seq;
            if current != before {
                break current;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("watcher did not process the change under the fd limit");
    let (_, updated) = handle
        .content(paired.device_token.clone(), document.node_id, None)
        .await
        .unwrap();
    let folder_source = notes.join(last_folder_name).join("Other.md");
    let nested = b"change in the final folder page\n";
    fs::write(folder_source, nested).unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let (_, bytes) = handle
                .content(paired.device_token.clone(), other.node_id, None)
                .await
                .unwrap();
            if bytes == nested {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("watcher did not detect the final page folder");
    runtime.shutdown().await.unwrap();
    assert_eq!(head.get(), before.get() + 1);
    assert_eq!(updated, external);
    assert_eq!(fs::read(source).unwrap(), external);
}

#[tokio::test]
async fn known_file_scan_reports_started_and_completed_for_successful_pass() {
    let rig = RuntimeRig::new().await;
    let handle = rig.runtime.handle();
    let events = handle.test_known_file_scan_events();
    let ready = handle.health().await.unwrap().ready;
    rig.runtime.shutdown().await.unwrap();
    assert_eq!(
        events,
        ["known_file_scan_started", "known_file_scan_complete"]
    );
    assert!(ready);
}

#[tokio::test]
async fn known_file_scan_without_watcher_completes_but_diagnostic_coverage_is_partial() {
    use notryx_protocol::admin::{AdminRequest, AdminResult};

    let rig = RuntimeRig::new().await;
    let handle = rig.runtime.handle();
    let incomplete = handle.known_file_scan_incomplete();
    let diagnostic = handle
        .admin(AdminRequest::UnsupportedItems {
            snapshot_id: None,
            offset: 0,
            limit: 20,
        })
        .await
        .unwrap();
    let events = handle.test_known_file_scan_events();
    let (_, bytes) = handle
        .content(rig.paired.device_token.clone(), rig.document.node_id, None)
        .await
        .unwrap();
    rig.runtime.shutdown().await.unwrap();

    assert!(!incomplete);
    assert!(matches!(
        diagnostic,
        AdminResult::UnsupportedItems {
            coverage_complete: false,
            ..
        }
    ));
    assert_eq!(
        events,
        ["known_file_scan_started", "known_file_scan_complete"]
    );
    assert_eq!(bytes, b"original document\n");
}

#[tokio::test]
async fn known_file_scan_preserves_missing_source_until_reconfirmation_then_tombstones() {
    use crate::config::WorkspaceConfig;
    use crate::runtime::WorkspaceRuntime;
    use std::time::Duration;

    let RuntimeRig {
        dir,
        runtime,
        document,
        other,
        paired,
    } = RuntimeRig::new().await;
    let handle = runtime.handle();
    let prior = SaveContent {
        epoch: paired.workspace_epoch,
        stream_id: paired.device_stream_id.parse().unwrap(),
        operation_seq: 1,
        operation_id: op_id(),
        node_id: other.node_id,
        expected_content_rev: other.content_rev.clone(),
        content_hash: digest(b"saved before restart\n"),
        bytes: b"saved before restart\n".to_vec(),
    };
    let receipt = handle
        .save_content(paired.device_token.clone(), prior.clone())
        .await
        .unwrap();
    let before = handle
        .server_info(paired.device_token.clone())
        .await
        .unwrap();
    runtime.shutdown().await.unwrap();
    let notes = dir.path().join("notes");
    let state = dir.path().join("state");
    let source = notes.join("Document.md");
    fs::remove_file(&source).unwrap();
    let (scan_entered, scan_entered_rx) = tokio::sync::oneshot::channel();
    let (scan_release, scan_release_rx) = tokio::sync::oneshot::channel();
    let mut runtime = WorkspaceRuntime::test_start_with_watch_control(
        WorkspaceConfig {
            notes_root: notes,
            state_root: state.clone(),
            uploads: Default::default(),
        },
        false,
        false,
        Some((scan_entered, scan_release_rx)),
    )
    .await
    .unwrap();
    let handle = runtime.handle();
    let (entered, release) = handle.test_pause_missing_confirmation();
    tokio::time::timeout(Duration::from_secs(10), scan_entered_rx)
        .await
        .unwrap()
        .unwrap();
    scan_release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), entered)
        .await
        .unwrap()
        .unwrap();
    let incomplete = handle.known_file_scan_incomplete();
    let info = handle
        .server_info(paired.device_token.clone())
        .await
        .unwrap();
    let node = handle
        .metadata(paired.device_token.clone(), document.node_id)
        .await
        .unwrap();
    let (stored, bytes) = handle
        .content(paired.device_token.clone(), document.node_id, None)
        .await
        .unwrap();
    let receipt_after = handle
        .operation(paired.device_token.clone(), prior.operation_id)
        .await
        .unwrap();
    let source_missing = !source.exists();
    let retained_object =
        fs::read(state.join("objects/sha256").join(&document.content_hash)).unwrap();
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), runtime.test_wait_startup_scan())
        .await
        .unwrap();
    let after = handle
        .server_info(paired.device_token.clone())
        .await
        .unwrap();
    let tombstone = handle
        .node_state(paired.device_token.clone(), document.node_id)
        .await
        .unwrap();
    let final_receipt = handle
        .operation(paired.device_token.clone(), prior.operation_id)
        .await
        .unwrap();
    let events = handle.test_known_file_scan_events();
    runtime.shutdown().await.unwrap();
    assert_eq!(
        events,
        ["known_file_scan_started", "known_file_scan_complete"]
    );
    assert!(incomplete);
    assert!(info.ready);
    assert_eq!(info.head_seq, before.head_seq);
    assert!(!node.deleted);
    assert_eq!(node.node_id, document.node_id);
    assert_eq!(stored, document);
    assert_eq!(bytes, b"original document\n");
    assert_eq!(retained_object, b"original document\n");
    assert_eq!(receipt_after, receipt);
    assert_eq!(final_receipt, receipt);
    assert_eq!(after.head_seq.get(), before.head_seq.get() + 1);
    assert!(tombstone.deleted);
    assert!(source_missing);
}

#[tokio::test]
async fn known_file_startup_scan_reaches_existing_file_after_first_keyset_page() {
    use crate::config::WorkspaceConfig;
    use crate::runtime::WorkspaceRuntime;
    use std::time::Duration;

    let RuntimeRig {
        dir,
        runtime,
        paired,
        ..
    } = RuntimeRig::new().await;
    runtime.shutdown().await.unwrap();
    let notes = dir.path().join("notes");
    let state = dir.path().join("state");
    let mut workspace = Workspace::open(&notes, &state).await.unwrap();
    let mut documents = Vec::new();
    for index in 0..70 {
        let name = format!("Page-{index:03}.md");
        fs::write(notes.join(&name), format!("original {index}\n")).unwrap();
        documents.push(workspace.adopt_root_document(&name).await.unwrap());
    }
    let first_page = workspace.state.inventory_nodes(None, 64).await.unwrap();
    assert_eq!(first_page.len(), 65);
    let page_boundary = first_page[63].node_id.to_string();
    let target = documents
        .into_iter()
        .find(|document| document.node_id.to_string() > page_boundary)
        .expect("fixture must include a file after the first page");
    let before = workspace.state.head_seq().await.unwrap();
    workspace.close().await.unwrap();
    let source = notes.join(&target.name);
    let external = b"changed after the first keyset page\n";
    fs::write(&source, external).unwrap();
    let mut runtime = WorkspaceRuntime::start(WorkspaceConfig {
        notes_root: notes,
        state_root: state,
        uploads: Default::default(),
    })
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(30), runtime.test_wait_startup_scan())
        .await
        .unwrap();
    let handle = runtime.handle();
    let incomplete = handle.known_file_scan_incomplete();
    let head = handle
        .server_info(paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let (changed, bytes) = handle
        .content(paired.device_token.clone(), target.node_id, None)
        .await
        .unwrap();
    let page = handle
        .changes(paired.device_token.clone(), before, Some(head))
        .await
        .unwrap();
    runtime.shutdown().await.unwrap();
    assert!(!incomplete);
    assert_eq!(head.get(), before.get() + 1);
    assert_eq!(changed.node_id, target.node_id);
    assert_eq!(changed.meta_rev, target.meta_rev);
    assert_ne!(changed.content_rev, target.content_rev);
    assert_eq!(bytes, external);
    assert_eq!(fs::read(source).unwrap(), external);
    assert_eq!(page.nodes.len(), 1);
    assert_eq!(page.nodes[0].node_id, target.node_id);
}

#[tokio::test]
async fn startup_known_file_scan_shutdown_preserves_data_and_releases_locks() {
    use crate::config::WorkspaceConfig;
    use crate::runtime::WorkspaceRuntime;
    use std::time::Duration;

    let RuntimeRig {
        dir,
        runtime,
        document,
        paired,
        ..
    } = RuntimeRig::new().await;
    let before = runtime
        .handle()
        .server_info(paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    runtime.shutdown().await.unwrap();
    let notes = dir.path().join("notes");
    let state = dir.path().join("state");
    let source = notes.join("Document.md");
    let external = b"external change during shutdown\n";
    fs::write(&source, external).unwrap();
    let runtime = WorkspaceRuntime::start(WorkspaceConfig {
        notes_root: notes.clone(),
        state_root: state.clone(),
        uploads: Default::default(),
    })
    .await
    .unwrap();
    let handle = runtime.handle();
    tokio::time::timeout(Duration::from_secs(5), async {
        while handle.test_observation_active() == 0 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("startup scan did not start an observation");
    assert!(matches!(
        Workspace::open(&notes, &state).await,
        Err(Error::Locked)
    ));
    tokio::time::timeout(Duration::from_secs(10), runtime.shutdown())
        .await
        .unwrap()
        .unwrap();
    let mut reopened = Workspace::open(&notes, &state).await.unwrap();
    let after = reopened.state.head_seq().await.unwrap();
    let (stored, bytes) = reopened.read(document.node_id).await.unwrap();
    assert!(reopened.state.pending().await.unwrap().is_empty());
    assert_eq!(fs::read(&source).unwrap(), external);
    assert_eq!(stored.node_id, document.node_id);
    assert_eq!(stored.meta_rev, document.meta_rev);
    if after == before {
        assert_eq!(bytes, b"original document\n");
        assert_eq!(stored.content_rev, document.content_rev);
    } else {
        assert_eq!(after.get(), before.get() + 1);
        assert_eq!(bytes, external);
        assert_ne!(stored.content_rev, document.content_rev);
    }
    reopened.close().await.unwrap();
}

type BlockedRuntimeObservation = (
    tokio::task::JoinHandle<Result<Option<Document>, crate::runtime::RuntimeError>>,
    tokio::sync::oneshot::Receiver<()>,
    std::sync::mpsc::Sender<()>,
);

fn blocked_runtime_observation(
    handle: crate::runtime::WorkspaceHandle,
    node_id: NodeId,
) -> BlockedRuntimeObservation {
    blocked_runtime_observation_at(handle, node_id, StorageStage::BeforeRecoveryFileSync)
}

fn blocked_runtime_observation_at(
    handle: crate::runtime::WorkspaceHandle,
    node_id: NodeId,
    blocked_stage: StorageStage,
) -> BlockedRuntimeObservation {
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let task = tokio::spawn(async move {
        let mut entered = Some(entered_tx);
        handle
            .test_reconcile_file(node_id, move |stage| {
                if stage == blocked_stage {
                    if let Some(entered) = entered.take() {
                        let _ = entered.send(());
                    }
                    release_rx
                        .recv_timeout(std::time::Duration::from_secs(20))
                        .map_err(|_| Error::Storage)?;
                }
                Ok(())
            })
            .await
    });
    (task, entered_rx, release_tx)
}

#[tokio::test]
async fn runtime_bounds_workers_keeps_api_responsive_and_discards_old_generation() {
    use crate::runtime::RuntimeError;
    use std::time::Duration;
    let rig = RuntimeRig::new().await;
    let handle = rig.runtime.handle();
    fs::write(
        rig.dir.path().join("notes/Document.md"),
        b"external document\n",
    )
    .unwrap();
    let (first, entered_first, release_first) =
        blocked_runtime_observation(handle.clone(), rig.document.node_id);
    tokio::time::timeout(Duration::from_secs(10), entered_first)
        .await
        .unwrap()
        .unwrap();
    let (second, entered_second, release_second) =
        blocked_runtime_observation(handle.clone(), rig.document.node_id);
    tokio::time::timeout(Duration::from_secs(10), entered_second)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        handle.reconcile_file(rig.document.node_id).await,
        Err(RuntimeError::Busy)
    );
    assert!(
        tokio::time::timeout(Duration::from_secs(5), handle.health())
            .await
            .unwrap()
            .unwrap()
            .ready
    );
    let request = SaveContent {
        epoch: rig.paired.workspace_epoch,
        stream_id: rig.paired.device_stream_id.parse().unwrap(),
        operation_seq: 1,
        operation_id: op_id(),
        node_id: rig.other.node_id,
        expected_content_rev: rig.other.content_rev.clone(),
        content_hash: digest(b"api save\n"),
        bytes: b"api save\n".to_vec(),
    };
    let saved = tokio::time::timeout(
        Duration::from_secs(10),
        handle.save_content(rig.paired.device_token.clone(), request),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(matches!(saved.outcome, Outcome::Saved(_)));
    let mut notices = handle.event_subscription();
    release_second.send(()).unwrap();
    let updated = tokio::time::timeout(Duration::from_secs(10), second)
        .await
        .unwrap()
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(updated.content_hash, digest(b"external document\n"));
    tokio::time::timeout(Duration::from_secs(5), notices.recv())
        .await
        .unwrap()
        .unwrap();
    release_first.send(()).unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(10), first)
            .await
            .unwrap()
            .unwrap(),
        Err(RuntimeError::Workspace(Error::ExternalChange))
    );
    assert_eq!(
        fs::read(rig.dir.path().join("notes/Other.md")).unwrap(),
        b"api save\n"
    );
    assert_eq!(
        fs::read(rig.dir.path().join("notes/Document.md")).unwrap(),
        b"external document\n"
    );
    rig.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn runtime_shutdown_drains_observation_workers_and_releases_locks() {
    use std::time::Duration;
    let rig = RuntimeRig::new().await;
    let handle = rig.runtime.handle();
    fs::write(
        rig.dir.path().join("notes/Document.md"),
        b"external document\n",
    )
    .unwrap();
    let (worker, entered, release) = blocked_runtime_observation(handle, rig.document.node_id);
    tokio::time::timeout(Duration::from_secs(10), entered)
        .await
        .unwrap()
        .unwrap();
    let mut shutting_down = tokio::spawn(async move { rig.runtime.shutdown().await });
    assert!(
        tokio::time::timeout(Duration::from_millis(200), &mut shutting_down)
            .await
            .is_err()
    );
    release.send(()).unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(10), worker)
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), shutting_down)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let reopened = Workspace::open(&rig.dir.path().join("notes"), &rig.dir.path().join("state"))
        .await
        .unwrap();
    reopened.close().await.unwrap();
}

fn corrupt_runtime_object(rig: &RuntimeRig) {
    use std::os::unix::fs::PermissionsExt;
    let object = rig
        .dir
        .path()
        .join("state/objects/sha256")
        .join(&rig.document.content_hash);
    fs::set_permissions(&object, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(&object, b"corrupt current object").unwrap();
    fs::set_permissions(&object, fs::Permissions::from_mode(0o400)).unwrap();
    fs::write(
        rig.dir.path().join("notes/Document.md"),
        b"external content\n",
    )
    .unwrap();
}

async fn await_runtime_not_ready(handle: &crate::runtime::WorkspaceHandle) -> bool {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if !handle.health().await.unwrap().ready {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .is_ok()
}

#[tokio::test]
async fn cancelled_observation_still_reports_referenced_object_corruption() {
    use std::time::Duration;
    let rig = RuntimeRig::new().await;
    let handle = rig.runtime.handle();
    corrupt_runtime_object(&rig);
    let (worker, entered, release) = blocked_runtime_observation_at(
        handle.clone(),
        rig.document.node_id,
        StorageStage::BeforeExternalObjectVerify,
    );
    tokio::time::timeout(Duration::from_secs(10), entered)
        .await
        .unwrap()
        .unwrap();
    worker.abort();
    release.send(()).unwrap();
    let diagnosed = await_runtime_not_ready(&handle).await;
    let source = fs::read(rig.dir.path().join("notes/Document.md")).unwrap();
    rig.runtime.shutdown().await.unwrap();
    assert!(
        diagnosed,
        "referenced object corruption must reach the coordinator"
    );
    assert_eq!(source, b"external content\n");
}

#[tokio::test]
async fn cancelled_queued_observation_still_reports_referenced_object_corruption() {
    use std::time::Duration;
    let rig = RuntimeRig::new().await;
    let handle = rig.runtime.handle();
    corrupt_runtime_object(&rig);
    let (worker, entered, release) = blocked_runtime_observation_at(
        handle.clone(),
        rig.document.node_id,
        StorageStage::BeforeExternalObjectVerify,
    );
    tokio::time::timeout(Duration::from_secs(10), entered)
        .await
        .unwrap()
        .unwrap();
    let barrier = handle.test_barrier().await;
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while handle.pending_commands() == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    worker.abort();
    barrier.send(()).unwrap();
    let diagnosed = await_runtime_not_ready(&handle).await;
    rig.runtime.shutdown().await.unwrap();
    assert!(
        diagnosed,
        "queued object corruption must reach the coordinator after cancellation"
    );
}

#[tokio::test]
async fn cancelled_valid_observation_does_not_commit() {
    use std::time::Duration;
    let rig = RuntimeRig::new().await;
    let handle = rig.runtime.handle();
    let before = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    fs::write(
        rig.dir.path().join("notes/Document.md"),
        b"external content\n",
    )
    .unwrap();
    let (worker, entered, release) =
        blocked_runtime_observation(handle.clone(), rig.document.node_id);
    tokio::time::timeout(Duration::from_secs(10), entered)
        .await
        .unwrap()
        .unwrap();
    worker.abort();
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while handle.test_observation_active() != 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let after = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    assert_eq!(after, before);
    assert!(handle.health().await.unwrap().ready);
    assert_eq!(
        fs::read(rig.dir.path().join("notes/Document.md")).unwrap(),
        b"external content\n"
    );
    rig.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancelled_panicking_worker_is_reported_and_can_be_retried() {
    use std::time::Duration;
    let rig = RuntimeRig::new().await;
    let handle = rig.runtime.handle();
    fs::write(
        rig.dir.path().join("notes/Document.md"),
        b"external content\n",
    )
    .unwrap();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let worker_handle = handle.clone();
    let node_id = rig.document.node_id;
    let worker = tokio::spawn(async move {
        let mut entered = Some(entered_tx);
        worker_handle
            .test_reconcile_file(node_id, move |stage| {
                if stage == StorageStage::BeforeRecoveryFileSync {
                    if let Some(entered) = entered.take() {
                        let _ = entered.send(());
                    }
                    release_rx
                        .recv_timeout(Duration::from_secs(15))
                        .map_err(|_| Error::Storage)?;
                    panic!("injected observation worker panic");
                }
                Ok(())
            })
            .await
    });
    tokio::time::timeout(Duration::from_secs(10), entered_rx)
        .await
        .unwrap()
        .unwrap();
    worker.abort();
    release_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !handle.observation_worker_failed() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let updated = handle
        .reconcile_file(rig.document.node_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.content_hash, digest(b"external content\n"));
    assert!(handle.health().await.unwrap().ready);
    rig.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancellation_after_transaction_start_preserves_atomic_commit_and_sse() {
    use std::time::Duration;
    let rig = RuntimeRig::new().await;
    let handle = rig.runtime.handle();
    let before = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let external = b"external after commit begins\n";
    fs::write(rig.dir.path().join("notes/Document.md"), external).unwrap();
    let mut notices = handle.event_subscription();
    let (entered, release) = handle.test_external_commit_pause().await;
    let node_id = rig.document.node_id;
    let worker_handle = handle.clone();
    let task = tokio::spawn(async move { worker_handle.reconcile_file(node_id).await });
    tokio::time::timeout(Duration::from_secs(10), entered)
        .await
        .unwrap()
        .unwrap();
    task.abort();
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), notices.recv())
        .await
        .unwrap()
        .unwrap();
    let after = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    assert_eq!(after.get(), before.get() + 1);
    let page = handle
        .changes(rig.paired.device_token.clone(), before, Some(after))
        .await
        .unwrap();
    assert_eq!(page.covered_through, after);
    assert_eq!(page.nodes.len(), 1);
    assert_eq!(page.nodes[0].node_id, rig.document.node_id);
    assert_eq!(
        page.nodes[0].content_hash.as_deref(),
        Some(format!("sha256:{}", digest(external)).as_str())
    );
    assert_eq!(
        handle
            .content(rig.paired.device_token.clone(), rig.document.node_id, None)
            .await
            .unwrap()
            .1,
        external
    );
    assert_eq!(
        fs::read(rig.dir.path().join("notes/Document.md")).unwrap(),
        external
    );
    rig.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn external_writer_during_sync_prevents_stale_commit() {
    let mut rig = Rig::new().await;
    let path = rig.dir.path().join("notes/Document.md");
    fs::write(&path, b"first external content\n").unwrap();
    let changed = path.clone();
    let candidate = rig
        .workspace
        .prepare_external_file(rig.initial.node_id)
        .await
        .unwrap()
        .observe_with_storage_hook(move |stage| {
            if stage == StorageStage::AfterRecoveryDirectorySync {
                fs::write(&changed, b"newer external content\n").unwrap();
            }
            Ok(())
        })
        .unwrap();
    assert_eq!(
        rig.workspace.commit_external_file(candidate).await,
        Err(Error::ExternalChange)
    );
    assert_eq!(
        rig.workspace.state.head_seq().await.unwrap(),
        rig.initial.last_change_seq
    );
    assert_eq!(
        rig.workspace.read(rig.initial.node_id).await.unwrap().0,
        rig.initial
    );
    assert_eq!(fs::read(path).unwrap(), b"newer external content\n");
}

#[tokio::test]
async fn external_discovery_records_source_and_parent_in_one_changeset() {
    let mut rig = Rig::new().await;
    let root = rig.workspace.state.root_id;
    let parent_before = rig.workspace.state.node(root).await.unwrap().unwrap();
    let head_before = rig.workspace.state.head_seq().await.unwrap();
    let original = fs::read(rig.dir.path().join("notes/Document.md")).unwrap();
    let path = rig.dir.path().join("notes/Discovered.md");
    let bytes = b"\xef\xbb\xbf# External\r\n\r\n<!-- preserved -->\r\n";
    fs::write(&path, bytes).unwrap();

    let candidate = rig
        .workspace
        .prepare_external_create(root, "Discovered.md")
        .await
        .unwrap()
        .observe()
        .unwrap();
    let created = rig
        .workspace
        .commit_external_create(candidate)
        .await
        .unwrap();
    assert_eq!(created.parent_id, Some(root));
    assert_eq!(created.name, "Discovered.md");
    assert_eq!(created.kind, notryx_protocol::NodeKind::File);
    assert!(!created.deleted);
    assert_eq!(
        created.content_hash.as_deref(),
        Some(digest(bytes).as_str())
    );
    assert_eq!(created.size, Some(bytes.len() as u64));
    assert_eq!(created.last_change_seq.get(), head_before.get() + 1);
    let parent_after = rig.workspace.state.node(root).await.unwrap().unwrap();
    assert_eq!(
        parent_after.children_generation,
        parent_before.children_generation + 1
    );
    assert_eq!(parent_after.meta_rev, parent_before.meta_rev);
    assert_eq!(parent_after.last_change_seq, created.last_change_seq);
    let page = rig.workspace.changes(head_before, None).await.unwrap();
    assert_eq!(page.covered_through, created.last_change_seq);
    assert_eq!(page.nodes.len(), 2);
    assert!(page.nodes.contains(&created.representation().unwrap()));
    assert!(page.nodes.contains(&parent_after.representation().unwrap()));
    assert_eq!(fs::read(&path).unwrap(), bytes);
    assert_eq!(rig.workspace.read(created.node_id).await.unwrap().1, bytes);
    assert_eq!(
        rig.workspace.read(rig.initial.node_id).await.unwrap().1,
        original
    );
    let receipts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM operation_receipts")
        .fetch_one(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    assert_eq!(receipts, 0);
    rig.workspace.close().await.unwrap();
    let mut reopened =
        Workspace::open(&rig.dir.path().join("notes"), &rig.dir.path().join("state"))
            .await
            .unwrap();
    assert_eq!(reopened.read(created.node_id).await.unwrap().1, bytes);
    assert_eq!(
        reopened.state.node(created.node_id).await.unwrap().unwrap(),
        created
    );
}

#[tokio::test]
async fn external_discovery_rejects_replaced_file_after_observation() {
    let mut rig = Rig::new().await;
    let root = rig.workspace.state.root_id;
    let before = rig.workspace.state.node(root).await.unwrap().unwrap();
    let head = rig.workspace.state.head_seq().await.unwrap();
    let path = rig.dir.path().join("notes/Discovered.md");
    let replacement = rig.dir.path().join("notes/Replacement.tmp");
    fs::write(&path, b"observed external bytes").unwrap();
    let candidate = rig
        .workspace
        .prepare_external_create(root, "Discovered.md")
        .await
        .unwrap()
        .observe()
        .unwrap();
    fs::write(&replacement, b"newer external bytes").unwrap();
    fs::rename(&replacement, &path).unwrap();
    assert_eq!(
        rig.workspace.commit_external_create(candidate).await,
        Err(Error::ExternalChange)
    );
    assert_eq!(rig.workspace.state.head_seq().await.unwrap(), head);
    assert_eq!(
        rig.workspace.state.node(root).await.unwrap().unwrap(),
        before
    );
    assert_eq!(fs::read(path).unwrap(), b"newer external bytes");
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM nodes WHERE parent_id=? AND name=?")
        .bind(root.to_string())
        .bind("Discovered.md")
        .fetch_one(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn external_discovery_sqlite_failure_preserves_untracked_source() {
    let mut rig = Rig::new().await;
    let root = rig.workspace.state.root_id;
    let before = rig.workspace.state.node(root).await.unwrap().unwrap();
    let head = rig.workspace.state.head_seq().await.unwrap();
    let path = rig.dir.path().join("notes/Discovered.md");
    fs::write(&path, b"external source stays").unwrap();
    let candidate = rig
        .workspace
        .prepare_external_create(root, "Discovered.md")
        .await
        .unwrap()
        .observe()
        .unwrap();
    sqlx::raw_sql("CREATE TRIGGER fail_external_create BEFORE INSERT ON node_changes BEGIN SELECT RAISE(FAIL,'injected'); END;")
        .execute(&mut rig.workspace.state.connection).await.unwrap();
    assert_eq!(
        rig.workspace.commit_external_create(candidate).await,
        Err(Error::Storage)
    );
    assert_eq!(rig.workspace.state.head_seq().await.unwrap(), head);
    assert_eq!(
        rig.workspace.state.node(root).await.unwrap().unwrap(),
        before
    );
    assert_eq!(fs::read(&path).unwrap(), b"external source stays");
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM nodes WHERE parent_id=? AND name=?")
        .bind(root.to_string())
        .bind("Discovered.md")
        .fetch_one(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    assert_eq!(count, 0);
    sqlx::raw_sql("DROP TRIGGER fail_external_create")
        .execute(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    rig.workspace.close().await.unwrap();
    let mut reopened =
        Workspace::open(&rig.dir.path().join("notes"), &rig.dir.path().join("state"))
            .await
            .unwrap();
    assert_eq!(reopened.state.head_seq().await.unwrap(), head);
    assert_eq!(reopened.state.node(root).await.unwrap().unwrap(), before);
    assert_eq!(fs::read(&path).unwrap(), b"external source stays");
    assert_eq!(
        reopened.read(rig.initial.node_id).await.unwrap().0,
        rig.initial
    );
}

#[tokio::test]
async fn external_discovery_rejects_symlink_and_preserves_target() {
    use std::os::unix::fs::symlink;
    let mut rig = Rig::new().await;
    let root = rig.workspace.state.root_id;
    let head = rig.workspace.state.head_seq().await.unwrap();
    let outside = rig.dir.path().join("outside.md");
    let link = rig.dir.path().join("notes/Link.md");
    fs::write(&outside, b"outside data").unwrap();
    symlink(&outside, &link).unwrap();
    let candidate = rig.workspace.prepare_external_create(root, "Link.md").await;
    if let Ok(probe) = candidate {
        assert!(probe.observe().is_err());
    }
    assert_eq!(rig.workspace.state.head_seq().await.unwrap(), head);
    assert_eq!(fs::read(&outside).unwrap(), b"outside data");
    assert!(fs::symlink_metadata(link).unwrap().file_type().is_symlink());
}

#[tokio::test]
async fn external_discovery_rejects_candidate_after_parent_move() {
    let mut rig = Rig::new().await;
    let root = rig.workspace.state.root_id;
    let request = CreateNode {
        epoch: rig.workspace.epoch(),
        stream_id: rig.stream,
        operation_seq: 1,
        operation_id: op_id(),
        node_id: node_id(),
        parent_id: root,
        name: "Folder".into(),
        kind: notryx_protocol::NodeKind::Folder,
        upload_id: None,
    };
    let Outcome::Created(folder) = rig.workspace.create_node(request).await.unwrap().outcome else {
        panic!("expected folder");
    };
    let source = rig.dir.path().join("notes/Folder/Discovered.md");
    fs::write(&source, b"keep moved bytes").unwrap();
    let candidate = rig
        .workspace
        .prepare_external_create(folder.node_id, "Discovered.md")
        .await
        .unwrap()
        .observe()
        .unwrap();
    let request = MoveNode {
        epoch: rig.workspace.epoch(),
        stream_id: rig.stream,
        operation_seq: 2,
        operation_id: op_id(),
        node_id: folder.node_id,
        expected_meta_rev: folder.meta_rev,
        expected_parent_id: root,
        parent_id: root,
        name: "Moved".into(),
    };
    assert!(matches!(
        rig.workspace.move_node(request).await.unwrap().outcome,
        Outcome::Moved(_)
    ));
    let head = rig.workspace.state.head_seq().await.unwrap();
    assert_eq!(
        rig.workspace.commit_external_create(candidate).await,
        Err(Error::ExternalChange)
    );
    assert_eq!(rig.workspace.state.head_seq().await.unwrap(), head);
    assert_eq!(
        fs::read(rig.dir.path().join("notes/Moved/Discovered.md")).unwrap(),
        b"keep moved bytes"
    );
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM nodes WHERE parent_id=? AND name=?")
        .bind(folder.node_id.to_string())
        .bind("Discovered.md")
        .fetch_one(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn external_discovery_stale_candidate_cannot_replace_api_created_node() {
    let mut rig = Rig::new().await;
    let root = rig.workspace.state.root_id;
    let path = rig.dir.path().join("notes/Discovered.md");
    fs::write(&path, b"old external source").unwrap();
    let candidate = rig
        .workspace
        .prepare_external_create(root, "Discovered.md")
        .await
        .unwrap()
        .observe()
        .unwrap();
    fs::remove_file(&path).unwrap();
    let request = CreateNode {
        epoch: rig.workspace.epoch(),
        stream_id: rig.stream,
        operation_seq: 1,
        operation_id: op_id(),
        node_id: node_id(),
        parent_id: root,
        name: "Discovered.md".into(),
        kind: notryx_protocol::NodeKind::File,
        upload_id: None,
    };
    let Outcome::Created(created) = rig.workspace.create_node(request).await.unwrap().outcome
    else {
        panic!("expected API-created file");
    };
    let head = rig.workspace.state.head_seq().await.unwrap();
    assert_eq!(
        rig.workspace.commit_external_create(candidate).await,
        Err(Error::ExternalChange)
    );
    assert_eq!(rig.workspace.state.head_seq().await.unwrap(), head);
    assert_eq!(
        rig.workspace
            .state
            .node(created.node_id)
            .await
            .unwrap()
            .unwrap(),
        created
    );
    assert_eq!(fs::read(path).unwrap(), b"");
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM nodes WHERE parent_id=? AND name=?")
        .bind(root.to_string())
        .bind("Discovered.md")
        .fetch_one(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn external_discovery_rejects_casefold_collision_without_selecting_a_winner() {
    let mut rig = Rig::new().await;
    let root = rig.workspace.state.root_id;
    let parent = rig.workspace.state.node(root).await.unwrap().unwrap();
    let head = rig.workspace.state.head_seq().await.unwrap();
    let first = rig.dir.path().join("notes/Note.md");
    let second = rig.dir.path().join("notes/note.md");
    fs::write(&first, b"first distinct source").unwrap();
    fs::write(&second, b"second distinct source").unwrap();
    for name in ["Note.md", "note.md"] {
        let result = match rig.workspace.prepare_external_create(root, name).await {
            Ok(probe) => match probe.observe() {
                Ok(candidate) => rig
                    .workspace
                    .commit_external_create(candidate)
                    .await
                    .map(|_| ()),
                Err(error) => Err(error),
            },
            Err(error) => Err(error),
        };
        assert!(result.is_err(), "ambiguous source must not become a node");
    }
    assert_eq!(rig.workspace.state.head_seq().await.unwrap(), head);
    assert_eq!(
        rig.workspace.state.node(root).await.unwrap().unwrap(),
        parent
    );
    assert_eq!(fs::read(&first).unwrap(), b"first distinct source");
    assert_eq!(fs::read(&second).unwrap(), b"second distinct source");
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM nodes WHERE parent_id=? AND normalized_name_key=?",
    )
    .bind(root.to_string())
    .bind(notryx_domain::mutation::normalized_name_key("Note.md"))
    .fetch_one(&mut rig.workspace.state.connection)
    .await
    .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn external_discovery_rejects_collision_created_after_observation() {
    let mut rig = Rig::new().await;
    let root = rig.workspace.state.root_id;
    let parent = rig.workspace.state.node(root).await.unwrap().unwrap();
    let head = rig.workspace.state.head_seq().await.unwrap();
    let first = rig.dir.path().join("notes/Note.md");
    let second = rig.dir.path().join("notes/note.md");
    fs::write(&first, b"first distinct source").unwrap();
    let candidate = rig
        .workspace
        .prepare_external_create(root, "Note.md")
        .await
        .unwrap()
        .observe()
        .unwrap();
    fs::write(&second, b"second distinct source").unwrap();
    assert_eq!(
        rig.workspace.commit_external_create(candidate).await,
        Err(Error::ExternalChange)
    );
    assert_eq!(rig.workspace.state.head_seq().await.unwrap(), head);
    assert_eq!(
        rig.workspace.state.node(root).await.unwrap().unwrap(),
        parent
    );
    assert_eq!(fs::read(&first).unwrap(), b"first distinct source");
    assert_eq!(fs::read(&second).unwrap(), b"second distinct source");
}

async fn external_discovery_test_folder(rig: &mut Rig) -> NodeId {
    let request = CreateNode {
        epoch: rig.workspace.epoch(),
        stream_id: rig.stream,
        operation_seq: 1,
        operation_id: op_id(),
        node_id: node_id(),
        parent_id: rig.workspace.state.root_id,
        name: "Folder".into(),
        kind: notryx_protocol::NodeKind::Folder,
        upload_id: None,
    };
    let Outcome::Created(folder) = rig.workspace.create_node(request).await.unwrap().outcome else {
        panic!("expected folder");
    };
    folder.node_id
}

#[tokio::test]
async fn external_discovery_rejects_replaced_notes_root_with_copied_marker() {
    let mut rig = Rig::new().await;
    let root = external_discovery_test_folder(&mut rig).await;
    let parent = rig.workspace.state.node(root).await.unwrap().unwrap();
    let head = rig.workspace.state.head_seq().await.unwrap();
    let notes = rig.dir.path().join("notes");
    let old_notes = rig.dir.path().join("notes-before-replacement");
    fs::write(
        notes.join("Folder/Discovered.md"),
        b"old root external bytes",
    )
    .unwrap();
    let candidate = rig
        .workspace
        .prepare_external_create(root, "Discovered.md")
        .await
        .unwrap()
        .observe()
        .unwrap();
    fs::rename(&notes, &old_notes).unwrap();
    fs::create_dir(&notes).unwrap();
    fs::copy(
        old_notes.join(".mdw-workspace.json"),
        notes.join(".mdw-workspace.json"),
    )
    .unwrap();
    fs::create_dir(notes.join("Folder")).unwrap();
    fs::write(
        notes.join("Folder/Discovered.md"),
        b"replacement root bytes",
    )
    .unwrap();
    assert!(
        rig.workspace
            .commit_external_create(candidate)
            .await
            .is_err()
    );
    assert_eq!(rig.workspace.state.head_seq().await.unwrap(), head);
    assert_eq!(
        rig.workspace.state.node(root).await.unwrap().unwrap(),
        parent
    );
    assert_eq!(
        fs::read(old_notes.join("Folder/Discovered.md")).unwrap(),
        b"old root external bytes"
    );
    assert_eq!(
        fs::read(notes.join("Folder/Discovered.md")).unwrap(),
        b"replacement root bytes"
    );
    assert_eq!(
        fs::read(old_notes.join("Document.md")).unwrap(),
        b"\xef\xbb\xbf# Original\r\n\r\n<!-- untouched -->\r\n"
    );
}

#[tokio::test]
async fn external_discovery_rejects_notes_root_symlink_even_if_it_points_to_original() {
    use std::os::unix::fs::symlink;
    let mut rig = Rig::new().await;
    let root = external_discovery_test_folder(&mut rig).await;
    let parent = rig.workspace.state.node(root).await.unwrap().unwrap();
    let head = rig.workspace.state.head_seq().await.unwrap();
    let notes = rig.dir.path().join("notes");
    let old_notes = rig.dir.path().join("notes-before-replacement");
    fs::write(notes.join("Folder/Discovered.md"), b"preserve root bytes").unwrap();
    let candidate = rig
        .workspace
        .prepare_external_create(root, "Discovered.md")
        .await
        .unwrap()
        .observe()
        .unwrap();
    fs::rename(&notes, &old_notes).unwrap();
    symlink(&old_notes, &notes).unwrap();
    assert!(
        rig.workspace
            .commit_external_create(candidate)
            .await
            .is_err()
    );
    assert_eq!(rig.workspace.state.head_seq().await.unwrap(), head);
    assert_eq!(
        rig.workspace.state.node(root).await.unwrap().unwrap(),
        parent
    );
    assert_eq!(
        fs::read(old_notes.join("Folder/Discovered.md")).unwrap(),
        b"preserve root bytes"
    );
    assert!(
        fs::symlink_metadata(notes)
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

#[tokio::test]
async fn replaced_notes_root_rejects_new_save_but_keeps_previous_receipt_available() {
    let mut rig = Rig::new().await;
    let first = rig.command(b"saved before root replacement\n");
    let receipt = rig.workspace.save(first.clone()).await.unwrap();
    let Outcome::Saved(saved) = &receipt.outcome else {
        panic!("expected prior saved receipt");
    };
    let request = SaveContent {
        epoch: rig.workspace.epoch(),
        stream_id: rig.stream,
        operation_seq: 2,
        operation_id: op_id(),
        node_id: saved.node_id,
        expected_content_rev: saved.content_rev.clone(),
        content_hash: digest(b"must not be written\n"),
        bytes: b"must not be written\n".to_vec(),
    };
    let head = rig.workspace.state.head_seq().await.unwrap();
    let notes = rig.dir.path().join("notes");
    let old_notes = rig.dir.path().join("notes-before-replacement");
    fs::rename(&notes, &old_notes).unwrap();
    fs::create_dir(&notes).unwrap();
    fs::copy(
        old_notes.join(".mdw-workspace.json"),
        notes.join(".mdw-workspace.json"),
    )
    .unwrap();
    fs::write(notes.join("Document.md"), b"replacement root bytes\n").unwrap();

    assert!(rig.workspace.save(request).await.is_err());
    assert_eq!(rig.workspace.state.head_seq().await.unwrap(), head);
    let info = rig.workspace.server_info().await.unwrap();
    assert_eq!(info.head_seq, head);
    assert!(!info.ready);
    assert_eq!(rig.workspace.save(first).await.unwrap(), receipt);
    assert_eq!(
        fs::read(old_notes.join("Document.md")).unwrap(),
        b"saved before root replacement\n"
    );
    assert_eq!(
        fs::read(notes.join("Document.md")).unwrap(),
        b"replacement root bytes\n"
    );
    let receipt_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM operation_receipts")
        .fetch_one(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    assert_eq!(receipt_count, 1);
    let last: String =
        sqlx::query_scalar("SELECT last_seq FROM operation_streams WHERE stream_id=?")
            .bind(rig.stream.to_string())
            .fetch_one(&mut rig.workspace.state.connection)
            .await
            .unwrap();
    assert_eq!(last, "1");
}

#[tokio::test]
async fn replaced_notes_root_rejects_new_create_without_consuming_sequence() {
    let mut rig = Rig::new().await;
    let root = rig.workspace.state.root_id;
    let parent = rig.workspace.state.node(root).await.unwrap().unwrap();
    let head = rig.workspace.state.head_seq().await.unwrap();
    let notes = rig.dir.path().join("notes");
    let old_notes = rig.dir.path().join("notes-before-replacement");
    fs::rename(&notes, &old_notes).unwrap();
    fs::create_dir(&notes).unwrap();
    fs::copy(
        old_notes.join(".mdw-workspace.json"),
        notes.join(".mdw-workspace.json"),
    )
    .unwrap();
    let request = CreateNode {
        epoch: rig.workspace.epoch(),
        stream_id: rig.stream,
        operation_seq: 1,
        operation_id: op_id(),
        node_id: node_id(),
        parent_id: root,
        name: "New.md".into(),
        kind: notryx_protocol::NodeKind::File,
        upload_id: None,
    };
    assert!(rig.workspace.create_node(request).await.is_err());
    assert_eq!(rig.workspace.state.head_seq().await.unwrap(), head);
    assert_eq!(rig.workspace.server_info().await.unwrap().head_seq, head);
    assert_eq!(
        rig.workspace.state.node(root).await.unwrap().unwrap(),
        parent
    );
    assert!(!old_notes.join("New.md").exists());
    assert!(!notes.join("New.md").exists());
    assert_eq!(
        fs::read(old_notes.join("Document.md")).unwrap(),
        b"\xef\xbb\xbf# Original\r\n\r\n<!-- untouched -->\r\n"
    );
    let receipt_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM operation_receipts")
        .fetch_one(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    assert_eq!(receipt_count, 0);
    let last: String =
        sqlx::query_scalar("SELECT last_seq FROM operation_streams WHERE stream_id=?")
            .bind(rig.stream.to_string())
            .fetch_one(&mut rig.workspace.state.connection)
            .await
            .unwrap();
    assert_eq!(last, "0");
}

#[tokio::test]
async fn notes_root_replacement_after_save_admission_keeps_pending_intent_and_bytes() {
    let mut rig = Rig::new().await;
    let request = rig.command(b"proposed operation bytes\n");
    let initial_bytes = fs::read(rig.dir.path().join("notes/Document.md")).unwrap();
    let head = rig.workspace.state.head_seq().await.unwrap();
    let notes = rig.dir.path().join("notes");
    let old_notes = rig.dir.path().join("notes-before-replacement");
    let hook_notes = notes.clone();
    let hook_old_notes = old_notes.clone();
    rig.workspace.hook = Some(Box::new(move |phase| {
        if phase == Phase::AfterAdmission {
            fs::rename(&hook_notes, &hook_old_notes).unwrap();
            fs::create_dir(&hook_notes).unwrap();
            fs::copy(
                hook_old_notes.join(".mdw-workspace.json"),
                hook_notes.join(".mdw-workspace.json"),
            )
            .unwrap();
            fs::write(hook_notes.join("Document.md"), b"replacement root bytes\n").unwrap();
        }
        Ok(())
    }));

    assert!(rig.workspace.save(request.clone()).await.is_err());
    assert!(!rig.workspace.ready());
    assert_eq!(rig.workspace.state.head_seq().await.unwrap(), head);
    assert_eq!(
        rig.workspace.save(request.clone()).await.unwrap().outcome,
        Outcome::Pending
    );
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM write_intents")
        .fetch_one(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    assert_eq!(count, 1);
    assert_eq!(
        rig.workspace
            .files
            .read_object(&rig.initial.content_hash)
            .unwrap(),
        initial_bytes
    );
    assert_eq!(
        rig.workspace
            .files
            .read_object(&request.content_hash)
            .unwrap(),
        request.bytes
    );
    assert_eq!(
        fs::read(old_notes.join("Document.md")).unwrap(),
        initial_bytes
    );
    assert_eq!(
        fs::read(notes.join("Document.md")).unwrap(),
        b"replacement root bytes\n"
    );
    rig.workspace.hook = None;
    rig.workspace.close().await.unwrap();
    let replacement = rig.dir.path().join("replacement-preserved");
    fs::rename(&notes, &replacement).unwrap();
    fs::rename(&old_notes, &notes).unwrap();
    let mut reopened = Workspace::open(&notes, &rig.dir.path().join("state"))
        .await
        .unwrap();
    assert_eq!(
        reopened.save(request).await.unwrap().outcome,
        Outcome::NotApplied
    );
    assert_eq!(reopened.state.head_seq().await.unwrap(), head);
    assert_eq!(fs::read(notes.join("Document.md")).unwrap(), initial_bytes);
    assert_eq!(
        fs::read(replacement.join("Document.md")).unwrap(),
        b"replacement root bytes\n"
    );
}

fn replace_notes_during_admission(rig: &mut Rig) -> (std::path::PathBuf, std::path::PathBuf) {
    let notes = rig.dir.path().join("notes");
    let old_notes = rig.dir.path().join("notes-before-replacement");
    let hook_notes = notes.clone();
    let hook_old_notes = old_notes.clone();
    rig.workspace.hook = Some(Box::new(move |phase| {
        if phase == Phase::AfterAdmission {
            fs::rename(&hook_notes, &hook_old_notes).unwrap();
            fs::create_dir(&hook_notes).unwrap();
            fs::copy(
                hook_old_notes.join(".mdw-workspace.json"),
                hook_notes.join(".mdw-workspace.json"),
            )
            .unwrap();
            fs::write(hook_notes.join("Unrelated.md"), b"replacement root bytes\n").unwrap();
        }
        Ok(())
    }));
    (notes, old_notes)
}

#[tokio::test]
async fn notes_root_replacement_after_create_admission_keeps_intent_pending() {
    let mut rig = Rig::new().await;
    let root = rig.workspace.state.root_id;
    let request = CreateNode {
        epoch: rig.workspace.epoch(),
        stream_id: rig.stream,
        operation_seq: 1,
        operation_id: op_id(),
        node_id: node_id(),
        parent_id: root,
        name: "New.md".into(),
        kind: notryx_protocol::NodeKind::File,
        upload_id: None,
    };
    let head = rig.workspace.state.head_seq().await.unwrap();
    let (notes, old_notes) = replace_notes_during_admission(&mut rig);
    assert!(rig.workspace.create_node(request.clone()).await.is_err());
    assert_eq!(
        rig.workspace
            .create_node(request.clone())
            .await
            .unwrap()
            .outcome,
        Outcome::Pending
    );
    assert_eq!(rig.workspace.state.head_seq().await.unwrap(), head);
    assert!(!old_notes.join("New.md").exists());
    assert!(!notes.join("New.md").exists());
    assert_eq!(
        fs::read(old_notes.join("Document.md")).unwrap(),
        b"\xef\xbb\xbf# Original\r\n\r\n<!-- untouched -->\r\n"
    );
    assert_eq!(
        fs::read(notes.join("Unrelated.md")).unwrap(),
        b"replacement root bytes\n"
    );
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM write_intents")
        .fetch_one(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    assert_eq!(count, 1);
    rig.workspace.hook = None;
    rig.workspace.close().await.unwrap();
    fs::rename(&notes, rig.dir.path().join("replacement-preserved")).unwrap();
    fs::rename(&old_notes, &notes).unwrap();
    let mut reopened = Workspace::open(&notes, &rig.dir.path().join("state"))
        .await
        .unwrap();
    assert_eq!(
        reopened.create_node(request).await.unwrap().outcome,
        Outcome::NotApplied
    );
    assert_eq!(reopened.state.head_seq().await.unwrap(), head);
}

#[tokio::test]
async fn notes_root_replacement_after_move_admission_keeps_intent_pending() {
    let mut rig = Rig::new().await;
    let root = rig.workspace.state.root_id;
    let original = NodeSnapshot::from(&rig.initial);
    let request = MoveNode {
        epoch: rig.workspace.epoch(),
        stream_id: rig.stream,
        operation_seq: 1,
        operation_id: op_id(),
        node_id: original.node_id,
        expected_meta_rev: original.meta_rev.clone(),
        expected_parent_id: root,
        parent_id: root,
        name: "Moved.md".into(),
    };
    let head = rig.workspace.state.head_seq().await.unwrap();
    let (notes, old_notes) = replace_notes_during_admission(&mut rig);
    assert!(rig.workspace.move_node(request.clone()).await.is_err());
    assert_eq!(
        rig.workspace
            .move_node(request.clone())
            .await
            .unwrap()
            .outcome,
        Outcome::Pending
    );
    assert_eq!(rig.workspace.state.head_seq().await.unwrap(), head);
    assert_eq!(
        rig.workspace.state.node(original.node_id).await.unwrap(),
        Some(original)
    );
    assert!(old_notes.join("Document.md").is_file());
    assert!(!old_notes.join("Moved.md").exists());
    assert_eq!(
        fs::read(notes.join("Unrelated.md")).unwrap(),
        b"replacement root bytes\n"
    );
    rig.workspace.hook = None;
    rig.workspace.close().await.unwrap();
    fs::rename(&notes, rig.dir.path().join("replacement-preserved")).unwrap();
    fs::rename(&old_notes, &notes).unwrap();
    let mut reopened = Workspace::open(&notes, &rig.dir.path().join("state"))
        .await
        .unwrap();
    assert_eq!(
        reopened.move_node(request).await.unwrap().outcome,
        Outcome::NotApplied
    );
    assert_eq!(reopened.state.head_seq().await.unwrap(), head);
}

#[tokio::test]
async fn notes_root_replacement_after_delete_admission_keeps_intent_pending() {
    let mut rig = Rig::new().await;
    let original = NodeSnapshot::from(&rig.initial);
    let request = DeleteNode {
        epoch: rig.workspace.epoch(),
        stream_id: rig.stream,
        operation_seq: 1,
        operation_id: op_id(),
        node_id: original.node_id,
        expected_node_etag: original.etag().unwrap(),
    };
    let head = rig.workspace.state.head_seq().await.unwrap();
    let (notes, old_notes) = replace_notes_during_admission(&mut rig);
    assert!(rig.workspace.delete_node(request.clone()).await.is_err());
    assert_eq!(
        rig.workspace
            .delete_node(request.clone())
            .await
            .unwrap()
            .outcome,
        Outcome::Pending
    );
    assert_eq!(rig.workspace.state.head_seq().await.unwrap(), head);
    assert_eq!(
        rig.workspace.state.node(original.node_id).await.unwrap(),
        Some(original)
    );
    assert!(old_notes.join("Document.md").is_file());
    assert_eq!(
        fs::read(notes.join("Unrelated.md")).unwrap(),
        b"replacement root bytes\n"
    );
    rig.workspace.hook = None;
    rig.workspace.close().await.unwrap();
    fs::rename(&notes, rig.dir.path().join("replacement-preserved")).unwrap();
    fs::rename(&old_notes, &notes).unwrap();
    let mut reopened = Workspace::open(&notes, &rig.dir.path().join("state"))
        .await
        .unwrap();
    assert_eq!(
        reopened.delete_node(request).await.unwrap().outcome,
        Outcome::NotApplied
    );
    assert_eq!(reopened.state.head_seq().await.unwrap(), head);
}

#[tokio::test]
async fn notes_root_replacement_after_restore_admission_keeps_intent_pending() {
    let mut rig = Rig::new().await;
    let original = NodeSnapshot::from(&rig.initial);
    let delete = DeleteNode {
        epoch: rig.workspace.epoch(),
        stream_id: rig.stream,
        operation_seq: 1,
        operation_id: op_id(),
        node_id: original.node_id,
        expected_node_etag: original.etag().unwrap(),
    };
    let Outcome::Deleted(tombstone) = rig.workspace.delete_node(delete).await.unwrap().outcome
    else {
        panic!("expected tombstone");
    };
    let root = rig.workspace.state.root_id;
    let request = RestoreNode {
        epoch: rig.workspace.epoch(),
        stream_id: rig.stream,
        operation_seq: 2,
        operation_id: op_id(),
        node_id: original.node_id,
        expected_meta_rev: tombstone.meta_rev.clone(),
        parent_id: root,
        name: "Document.md".into(),
    };
    let head = rig.workspace.state.head_seq().await.unwrap();
    let (notes, old_notes) = replace_notes_during_admission(&mut rig);
    assert!(rig.workspace.restore_node(request.clone()).await.is_err());
    assert_eq!(
        rig.workspace
            .restore_node(request.clone())
            .await
            .unwrap()
            .outcome,
        Outcome::Pending
    );
    assert_eq!(rig.workspace.state.head_seq().await.unwrap(), head);
    assert_eq!(
        rig.workspace.state.node(original.node_id).await.unwrap(),
        Some(tombstone)
    );
    assert!(!old_notes.join("Document.md").exists());
    assert_eq!(
        rig.workspace
            .files
            .read_object(&rig.initial.content_hash)
            .unwrap(),
        b"\xef\xbb\xbf# Original\r\n\r\n<!-- untouched -->\r\n"
    );
    assert_eq!(
        fs::read(notes.join("Unrelated.md")).unwrap(),
        b"replacement root bytes\n"
    );
    rig.workspace.hook = None;
    rig.workspace.close().await.unwrap();
    fs::rename(&notes, rig.dir.path().join("replacement-preserved")).unwrap();
    fs::rename(&old_notes, &notes).unwrap();
    let mut reopened = Workspace::open(&notes, &rig.dir.path().join("state"))
        .await
        .unwrap();
    assert_eq!(
        reopened.restore_node(request).await.unwrap().outcome,
        Outcome::NotApplied
    );
    assert_eq!(reopened.state.head_seq().await.unwrap(), head);
}

fn blocked_runtime_new_file(
    handle: crate::runtime::WorkspaceHandle,
    parent_id: NodeId,
    name: &str,
    blocked_stage: StorageStage,
) -> (
    tokio::task::JoinHandle<Result<NodeSnapshot, crate::runtime::RuntimeError>>,
    tokio::sync::oneshot::Receiver<()>,
    std::sync::mpsc::Sender<()>,
) {
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let name = name.to_owned();
    let task = tokio::spawn(async move {
        let mut entered = Some(entered_tx);
        handle
            .test_reconcile_new_file(parent_id, name, move |stage| {
                if stage == blocked_stage {
                    if let Some(entered) = entered.take() {
                        let _ = entered.send(());
                    }
                    release_rx
                        .recv_timeout(std::time::Duration::from_secs(20))
                        .map_err(|_| Error::Storage)?;
                }
                Ok(())
            })
            .await
    });
    (task, entered_rx, release_tx)
}

fn blocked_new_file_with_recheck(
    handle: crate::runtime::WorkspaceHandle,
    parent_id: NodeId,
    recheck_hook: impl FnMut(crate::runtime::ExternalObjectRecheckStage) -> Result<(), Error>
    + Send
    + 'static,
) -> (
    tokio::task::JoinHandle<Result<NodeSnapshot, crate::runtime::RuntimeError>>,
    tokio::sync::oneshot::Receiver<()>,
    std::sync::mpsc::Sender<()>,
) {
    blocked_named_new_file_with_recheck(handle, parent_id, "New.md", recheck_hook)
}

fn blocked_named_new_file_with_recheck(
    handle: crate::runtime::WorkspaceHandle,
    parent_id: NodeId,
    name: &str,
    recheck_hook: impl FnMut(crate::runtime::ExternalObjectRecheckStage) -> Result<(), Error>
    + Send
    + 'static,
) -> (
    tokio::task::JoinHandle<Result<NodeSnapshot, crate::runtime::RuntimeError>>,
    tokio::sync::oneshot::Receiver<()>,
    std::sync::mpsc::Sender<()>,
) {
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let name = name.to_owned();
    let worker = tokio::spawn(async move {
        let mut entered = Some(entered_tx);
        handle
            .test_reconcile_new_file_with_recheck(
                parent_id,
                name,
                move |stage| {
                    if stage == StorageStage::BeforeObjectFileSync {
                        let _ = entered.take().unwrap().send(());
                        release_rx
                            .recv_timeout(std::time::Duration::from_secs(20))
                            .map_err(|_| Error::Storage)?;
                    }
                    Ok(())
                },
                recheck_hook,
            )
            .await
    });
    (worker, entered_rx, release_tx)
}

#[tokio::test]
async fn cancelled_recheck_revalidates_object_replaced_after_hash() {
    use crate::runtime::ExternalObjectRecheckStage;
    use std::time::Duration;
    let rig = RuntimeRig::new().await;
    let handle = rig.runtime.handle();
    let info = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap();
    let source = rig.dir.path().join("notes/New.md");
    let object = rig
        .dir
        .path()
        .join("state/objects/sha256")
        .join(&rig.document.content_hash);
    let valid = b"original document\n";
    let corrupt = b"corrupt document!\n";
    fs::write(&source, valid).unwrap();
    fs::remove_file(&object).unwrap();
    let mut notices = handle.event_subscription();
    let (verified_tx, verified_rx) = tokio::sync::oneshot::channel();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel();
    let mut verified = Some(verified_tx);
    let (worker, first_entered, first_release) =
        blocked_new_file_with_recheck(handle.clone(), info.root_id, move |stage| {
            if stage == ExternalObjectRecheckStage::AfterVerify
                && let Some(verified) = verified.take()
            {
                let _ = verified.send(());
                resume_rx
                    .recv_timeout(Duration::from_secs(20))
                    .map_err(|_| Error::Storage)?;
            }
            Ok(())
        });
    tokio::time::timeout(Duration::from_secs(10), first_entered)
        .await
        .unwrap()
        .unwrap();
    fs::write(&object, valid).unwrap();
    first_release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), verified_rx)
        .await
        .expect("object recheck did not run")
        .unwrap();
    worker.abort();
    let replacement = rig.dir.path().join("corrupt-object");
    fs::write(&replacement, corrupt).unwrap();
    fs::rename(replacement, &object).unwrap();
    resume_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while handle.test_observation_active() != 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let ready = handle.health().await.unwrap().ready;
    let head = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let source_bytes = fs::read(source).unwrap();
    let object_bytes = fs::read(object).unwrap();
    let no_notice = matches!(
        notices.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    );
    rig.runtime.shutdown().await.unwrap();
    assert!(
        !ready,
        "a changed referenced object must be verified again after cancellation"
    );
    assert_eq!(head, info.head_seq);
    assert_eq!(source_bytes, valid);
    assert_eq!(object_bytes, corrupt);
    assert!(no_notice);
}

#[tokio::test]
async fn referenced_object_recheck_io_failure_blocks_mutations() {
    use crate::runtime::ExternalObjectRecheckStage;
    use std::time::Duration;
    let rig = RuntimeRig::new().await;
    let handle = rig.runtime.handle();
    let info = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap();
    let source = rig.dir.path().join("notes/New.md");
    let object = rig
        .dir
        .path()
        .join("state/objects/sha256")
        .join(&rig.document.content_hash);
    let bytes = b"original document\n";
    fs::write(&source, bytes).unwrap();
    fs::remove_file(&object).unwrap();
    let mut notices = handle.event_subscription();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let mut entered = Some(entered_tx);
    let (worker, first_entered, first_release) =
        blocked_new_file_with_recheck(handle.clone(), info.root_id, move |stage| {
            if stage == ExternalObjectRecheckStage::BeforeVerify {
                let _ = entered.take().unwrap().send(());
                return Err(Error::Storage);
            }
            Ok(())
        });
    tokio::time::timeout(Duration::from_secs(10), first_entered)
        .await
        .unwrap()
        .unwrap();
    fs::write(&object, bytes).unwrap();
    first_release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), entered_rx)
        .await
        .expect("object recheck did not run")
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(10), worker)
        .await
        .unwrap()
        .unwrap();
    let ready = handle.health().await.unwrap().ready;
    let head = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let source_bytes = fs::read(source).unwrap();
    let object_bytes = fs::read(object).unwrap();
    let no_notice = matches!(
        notices.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    );
    rig.runtime.shutdown().await.unwrap();
    assert!(matches!(
        result,
        Err(crate::runtime::RuntimeError::Workspace(Error::Storage))
    ));
    assert!(!ready);
    assert_eq!(head, info.head_seq);
    assert_eq!(source_bytes, bytes);
    assert_eq!(object_bytes, bytes);
    assert!(no_notice);
}

#[tokio::test]
async fn shutdown_drains_cancelled_referenced_object_recheck() {
    use crate::runtime::ExternalObjectRecheckStage;
    use std::time::Duration;
    let rig = RuntimeRig::new().await;
    let handle = rig.runtime.handle();
    let info = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap();
    let notes = rig.dir.path().join("notes");
    let state = rig.dir.path().join("state");
    let source = notes.join("New.md");
    let object = state
        .join("objects/sha256")
        .join(&rig.document.content_hash);
    let bytes = b"original document\n";
    fs::write(&source, bytes).unwrap();
    fs::remove_file(&object).unwrap();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel();
    let mut entered = Some(entered_tx);
    let (worker, first_entered, first_release) =
        blocked_new_file_with_recheck(handle, info.root_id, move |stage| {
            if stage == ExternalObjectRecheckStage::BeforeVerify
                && let Some(entered) = entered.take()
            {
                let _ = entered.send(());
                resume_rx
                    .recv_timeout(Duration::from_secs(20))
                    .map_err(|_| Error::Storage)?;
            }
            Ok(())
        });
    tokio::time::timeout(Duration::from_secs(10), first_entered)
        .await
        .unwrap()
        .unwrap();
    fs::write(&object, bytes).unwrap();
    first_release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), entered_rx)
        .await
        .expect("object recheck did not run")
        .unwrap();
    worker.abort();
    let mut shutdown = tokio::spawn(async move { rig.runtime.shutdown().await });
    assert!(
        tokio::time::timeout(Duration::from_millis(200), &mut shutdown)
            .await
            .is_err()
    );
    assert!(matches!(
        Workspace::open(&notes, &state).await,
        Err(Error::Locked)
    ));
    resume_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), shutdown)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let mut reopened = Workspace::open(&notes, &state).await.unwrap();
    assert_eq!(reopened.state.head_seq().await.unwrap(), info.head_seq);
    assert_eq!(fs::read(source).unwrap(), bytes);
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn successful_recheck_does_not_clear_another_recheck_failure() {
    use crate::runtime::{ExternalObjectRecheckStage, RuntimeError};
    use std::time::Duration;
    let rig = RuntimeRig::new().await;
    let handle = rig.runtime.handle();
    let info = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap();
    let first_source = rig.dir.path().join("notes/New.md");
    let second_source = rig.dir.path().join("notes/New2.md");
    let first_bytes = b"original document\n";
    let second_bytes = b"other original\n";
    fs::write(&first_source, first_bytes).unwrap();
    fs::write(&second_source, second_bytes).unwrap();
    let objects = rig.dir.path().join("state/objects/sha256");
    let first_object = objects.join(&rig.document.content_hash);
    let second_object = objects.join(&rig.other.content_hash);
    fs::remove_file(&first_object).unwrap();
    fs::remove_file(&second_object).unwrap();
    let (first_entered_tx, first_entered_rx) = tokio::sync::oneshot::channel();
    let (first_resume_tx, first_resume_rx) = std::sync::mpsc::channel();
    let mut first_entered = Some(first_entered_tx);
    let (first, first_observed, first_release) =
        blocked_new_file_with_recheck(handle.clone(), info.root_id, move |stage| {
            if stage == ExternalObjectRecheckStage::BeforeVerify {
                let _ = first_entered.take().unwrap().send(());
                first_resume_rx
                    .recv_timeout(Duration::from_secs(20))
                    .map_err(|_| Error::Storage)?;
                return Err(Error::Storage);
            }
            Ok(())
        });
    let (second_entered_tx, second_entered_rx) = tokio::sync::oneshot::channel();
    let (second_resume_tx, second_resume_rx) = std::sync::mpsc::channel();
    let mut second_entered = Some(second_entered_tx);
    let (second, second_observed, second_release) = blocked_named_new_file_with_recheck(
        handle.clone(),
        info.root_id,
        "New2.md",
        move |stage| {
            if stage == ExternalObjectRecheckStage::BeforeVerify
                && let Some(entered) = second_entered.take()
            {
                let _ = entered.send(());
                second_resume_rx
                    .recv_timeout(Duration::from_secs(20))
                    .map_err(|_| Error::Storage)?;
            }
            Ok(())
        },
    );
    tokio::time::timeout(Duration::from_secs(10), first_observed)
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), second_observed)
        .await
        .unwrap()
        .unwrap();
    fs::write(&first_object, first_bytes).unwrap();
    fs::write(&second_object, second_bytes).unwrap();
    first_release.send(()).unwrap();
    second_release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), first_entered_rx)
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), second_entered_rx)
        .await
        .unwrap()
        .unwrap();
    assert!(!handle.health().await.unwrap().ready);
    first_resume_tx.send(()).unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(10), first)
            .await
            .unwrap()
            .unwrap(),
        Err(RuntimeError::Workspace(Error::Storage))
    );
    assert!(!handle.health().await.unwrap().ready);
    second_resume_tx.send(()).unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(10), second)
            .await
            .unwrap()
            .unwrap(),
        Err(RuntimeError::Workspace(Error::ExternalChange))
    );
    let ready = handle.health().await.unwrap().ready;
    let head = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let first_source_bytes = fs::read(first_source).unwrap();
    let second_source_bytes = fs::read(second_source).unwrap();
    rig.runtime.shutdown().await.unwrap();
    assert!(
        !ready,
        "successful recheck must not clear a different failure"
    );
    assert_eq!(head, info.head_seq);
    assert_eq!(first_source_bytes, first_bytes);
    assert_eq!(second_source_bytes, second_bytes);
}

#[tokio::test]
async fn pending_object_recheck_blocks_new_mutation_but_allows_receipt_lookup() {
    use crate::runtime::{ExternalObjectRecheckStage, RuntimeError};
    use std::time::Duration;
    let rig = RuntimeRig::new().await;
    let handle = rig.runtime.handle();
    let prior = SaveContent {
        epoch: rig.paired.workspace_epoch,
        stream_id: rig.paired.device_stream_id.parse().unwrap(),
        operation_seq: 1,
        operation_id: op_id(),
        node_id: rig.other.node_id,
        expected_content_rev: rig.other.content_rev.clone(),
        content_hash: digest(b"earlier edit\n"),
        bytes: b"earlier edit\n".to_vec(),
    };
    let receipt = handle
        .save_content(rig.paired.device_token.clone(), prior.clone())
        .await
        .unwrap();
    let info = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap();
    let source = rig.dir.path().join("notes/New.md");
    let object = rig
        .dir
        .path()
        .join("state/objects/sha256")
        .join(&rig.document.content_hash);
    let bytes = b"original document\n";
    fs::write(&source, bytes).unwrap();
    fs::remove_file(&object).unwrap();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel();
    let mut entered = Some(entered_tx);
    let (worker, observed, release) =
        blocked_new_file_with_recheck(handle.clone(), info.root_id, move |stage| {
            if stage == ExternalObjectRecheckStage::BeforeVerify
                && let Some(entered) = entered.take()
            {
                let _ = entered.send(());
                resume_rx
                    .recv_timeout(Duration::from_secs(20))
                    .map_err(|_| Error::Storage)?;
            }
            Ok(())
        });
    tokio::time::timeout(Duration::from_secs(10), observed)
        .await
        .unwrap()
        .unwrap();
    fs::write(&object, bytes).unwrap();
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), entered_rx)
        .await
        .unwrap()
        .unwrap();
    assert!(!handle.health().await.unwrap().ready);
    assert_eq!(
        handle
            .operation(rig.paired.device_token.clone(), prior.operation_id)
            .await
            .unwrap(),
        receipt
    );
    let next = SaveContent {
        epoch: rig.paired.workspace_epoch,
        stream_id: rig.paired.device_stream_id.parse().unwrap(),
        operation_seq: 2,
        operation_id: op_id(),
        node_id: rig.document.node_id,
        expected_content_rev: rig.document.content_rev.clone(),
        content_hash: digest(b"later edit\n"),
        bytes: b"later edit\n".to_vec(),
    };
    let blocked = handle
        .save_content(rig.paired.device_token.clone(), next)
        .await;
    let head = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    resume_tx.send(()).unwrap();
    let result = tokio::time::timeout(Duration::from_secs(10), worker)
        .await
        .unwrap()
        .unwrap();
    let ready = handle.health().await.unwrap().ready;
    let source_bytes = fs::read(source).unwrap();
    rig.runtime.shutdown().await.unwrap();
    assert_eq!(
        blocked,
        Err(RuntimeError::Workspace(Error::RecoveryRequired))
    );
    assert_eq!(result, Err(RuntimeError::Workspace(Error::ExternalChange)));
    assert_eq!(head, info.head_seq);
    assert_eq!(source_bytes, bytes);
    assert!(ready);
}

#[tokio::test]
async fn repeated_object_replacement_exhausts_rechecks_and_blocks_mutations() {
    use crate::runtime::{ExternalObjectRecheckStage, RuntimeError};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use std::time::Duration;
    let rig = RuntimeRig::new().await;
    let handle = rig.runtime.handle();
    let info = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap();
    let source = rig.dir.path().join("notes/New.md");
    let object = rig
        .dir
        .path()
        .join("state/objects/sha256")
        .join(&rig.document.content_hash);
    let replacement = rig.dir.path().join("replacement-object");
    let bytes = b"original document\n";
    fs::write(&source, bytes).unwrap();
    fs::remove_file(&object).unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let observed_count = Arc::clone(&count);
    let object_for_hook = object.clone();
    let (worker, observed, release) =
        blocked_new_file_with_recheck(handle.clone(), info.root_id, move |stage| {
            if stage == ExternalObjectRecheckStage::AfterVerify {
                fs::write(&replacement, bytes).map_err(|_| Error::Storage)?;
                fs::rename(&replacement, &object_for_hook).map_err(|_| Error::Storage)?;
                observed_count.fetch_add(1, Ordering::SeqCst);
            }
            Ok(())
        });
    tokio::time::timeout(Duration::from_secs(10), observed)
        .await
        .unwrap()
        .unwrap();
    fs::write(&object, bytes).unwrap();
    release.send(()).unwrap();
    let result = tokio::time::timeout(Duration::from_secs(10), worker)
        .await
        .unwrap()
        .unwrap();
    let ready = handle.health().await.unwrap().ready;
    let head = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let source_bytes = fs::read(source).unwrap();
    let object_bytes = fs::read(object).unwrap();
    rig.runtime.shutdown().await.unwrap();
    assert_eq!(result, Err(RuntimeError::Workspace(Error::Storage)));
    assert_eq!(count.load(Ordering::SeqCst), 3);
    assert!(!ready);
    assert_eq!(head, info.head_seq);
    assert_eq!(source_bytes, bytes);
    assert_eq!(object_bytes, bytes);
}

#[tokio::test]
async fn runtime_new_file_commits_journal_and_sends_sse_hint() {
    use std::time::Duration;
    let rig = RuntimeRig::new().await;
    let handle = rig.runtime.handle();
    let info = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap();
    let bytes = b"\xef\xbb\xbf# External\r\n";
    fs::write(rig.dir.path().join("notes/New.md"), bytes).unwrap();
    let mut notices = handle.event_subscription();
    let created = handle
        .reconcile_new_file(info.root_id, "New.md".into())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), notices.recv())
        .await
        .unwrap()
        .unwrap();
    let after = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap();
    assert_eq!(after.head_seq.get(), info.head_seq.get() + 1);
    let page = handle
        .changes(
            rig.paired.device_token.clone(),
            info.head_seq,
            Some(after.head_seq),
        )
        .await
        .unwrap();
    assert_eq!(page.nodes.len(), 2);
    assert!(
        page.nodes
            .iter()
            .any(|node| node.node_id == created.node_id)
    );
    assert!(page.nodes.iter().any(|node| node.node_id == info.root_id));
    assert_eq!(
        handle
            .content(rig.paired.device_token.clone(), created.node_id, None)
            .await
            .unwrap()
            .1,
        bytes
    );
    assert_eq!(
        fs::read(rig.dir.path().join("notes/New.md")).unwrap(),
        bytes
    );
    assert!(matches!(
        notices.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));
    rig.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn runtime_new_file_bounds_workers_and_discards_old_generation() {
    use crate::runtime::RuntimeError;
    use std::time::Duration;
    let rig = RuntimeRig::new().await;
    let handle = rig.runtime.handle();
    let info = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap();
    fs::write(rig.dir.path().join("notes/New.md"), b"external bytes\n").unwrap();
    let (first, entered_first, release_first) = blocked_runtime_new_file(
        handle.clone(),
        info.root_id,
        "New.md",
        StorageStage::BeforeRecoveryFileSync,
    );
    tokio::time::timeout(Duration::from_secs(10), entered_first)
        .await
        .unwrap()
        .unwrap();
    let (second, entered_second, release_second) = blocked_runtime_new_file(
        handle.clone(),
        info.root_id,
        "New.md",
        StorageStage::BeforeRecoveryFileSync,
    );
    tokio::time::timeout(Duration::from_secs(10), entered_second)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        handle
            .reconcile_new_file(info.root_id, "New.md".into())
            .await,
        Err(RuntimeError::Busy)
    );
    assert!(
        tokio::time::timeout(Duration::from_secs(5), handle.health())
            .await
            .unwrap()
            .unwrap()
            .ready
    );
    let mut notices = handle.event_subscription();
    release_second.send(()).unwrap();
    let created = tokio::time::timeout(Duration::from_secs(10), second)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), notices.recv())
        .await
        .unwrap()
        .unwrap();
    release_first.send(()).unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(10), first)
            .await
            .unwrap()
            .unwrap(),
        Err(RuntimeError::Workspace(Error::ExternalChange))
    );
    let after = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap();
    assert_eq!(after.head_seq.get(), info.head_seq.get() + 1);
    assert_eq!(
        handle
            .content(rig.paired.device_token.clone(), created.node_id, None)
            .await
            .unwrap()
            .1,
        b"external bytes\n"
    );
    assert_eq!(
        fs::read(rig.dir.path().join("notes/New.md")).unwrap(),
        b"external bytes\n"
    );
    rig.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancelled_new_file_observation_still_diagnoses_referenced_corruption() {
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;
    let rig = RuntimeRig::new().await;
    let handle = rig.runtime.handle();
    let info = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap();
    let path = rig.dir.path().join("notes/New.md");
    fs::write(&path, b"original document\n").unwrap();
    let object = rig
        .dir
        .path()
        .join("state/objects/sha256")
        .join(&rig.document.content_hash);
    fs::set_permissions(&object, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(&object, b"corrupt referenced object").unwrap();
    fs::set_permissions(&object, fs::Permissions::from_mode(0o400)).unwrap();
    let (worker, entered, release) = blocked_runtime_new_file(
        handle.clone(),
        info.root_id,
        "New.md",
        StorageStage::BeforeExternalObjectVerify,
    );
    tokio::time::timeout(Duration::from_secs(10), entered)
        .await
        .unwrap()
        .unwrap();
    worker.abort();
    release.send(()).unwrap();
    let diagnosed = await_runtime_not_ready(&handle).await;
    assert!(
        diagnosed,
        "referenced corruption must reach the coordinator after cancellation"
    );
    assert_eq!(
        handle
            .server_info(rig.paired.device_token.clone())
            .await
            .unwrap()
            .head_seq,
        info.head_seq
    );
    assert_eq!(fs::read(path).unwrap(), b"original document\n");
    assert_eq!(fs::read(object).unwrap(), b"corrupt referenced object");
    rig.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn runtime_shutdown_drains_new_file_worker_and_keeps_workspace_locked() {
    use std::time::Duration;
    let rig = RuntimeRig::new().await;
    let handle = rig.runtime.handle();
    let info = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap();
    let notes = rig.dir.path().join("notes");
    let state = rig.dir.path().join("state");
    fs::write(notes.join("New.md"), b"external bytes\n").unwrap();
    let (worker, entered, release) = blocked_runtime_new_file(
        handle,
        info.root_id,
        "New.md",
        StorageStage::BeforeRecoveryFileSync,
    );
    tokio::time::timeout(Duration::from_secs(10), entered)
        .await
        .unwrap()
        .unwrap();
    let mut shutdown = tokio::spawn(async move { rig.runtime.shutdown().await });
    assert!(
        tokio::time::timeout(Duration::from_millis(200), &mut shutdown)
            .await
            .is_err()
    );
    assert!(matches!(
        Workspace::open(&notes, &state).await,
        Err(Error::Locked)
    ));
    release.send(()).unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(10), worker)
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), shutdown)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let mut reopened = Workspace::open(&notes, &state).await.unwrap();
    assert_eq!(reopened.state.head_seq().await.unwrap(), info.head_seq);
    assert_eq!(fs::read(notes.join("New.md")).unwrap(), b"external bytes\n");
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn cancelled_new_file_observation_reports_unsafe_referenced_object() {
    use std::time::Duration;
    let rig = RuntimeRig::new().await;
    let handle = rig.runtime.handle();
    let info = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap();
    let path = rig.dir.path().join("notes/New.md");
    fs::write(&path, b"original document\n").unwrap();
    let object = rig
        .dir
        .path()
        .join("state/objects/sha256")
        .join(&rig.document.content_hash);
    let linked = rig.dir.path().join("unsafe-hardlink-copy");
    fs::hard_link(&object, &linked).unwrap();
    let (worker, entered, release) = blocked_runtime_new_file(
        handle.clone(),
        info.root_id,
        "New.md",
        StorageStage::BeforeExternalObjectVerify,
    );
    tokio::time::timeout(Duration::from_secs(10), entered)
        .await
        .unwrap()
        .unwrap();
    worker.abort();
    release.send(()).unwrap();
    assert!(
        await_runtime_not_ready(&handle).await,
        "unsafe referenced object must be diagnosed even after cancellation"
    );
    assert_eq!(
        handle
            .server_info(rig.paired.device_token.clone())
            .await
            .unwrap()
            .head_seq,
        info.head_seq
    );
    assert_eq!(fs::read(path).unwrap(), b"original document\n");
    assert_eq!(fs::read(&object).unwrap(), b"original document\n");
    assert_eq!(fs::read(&linked).unwrap(), b"original document\n");
    rig.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancelled_new_file_observation_reports_missing_referenced_object() {
    use std::time::Duration;
    let rig = RuntimeRig::new().await;
    let handle = rig.runtime.handle();
    let info = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap();
    let path = rig.dir.path().join("notes/New.md");
    fs::write(&path, b"original document\n").unwrap();
    let object = rig
        .dir
        .path()
        .join("state/objects/sha256")
        .join(&rig.document.content_hash);
    fs::remove_file(&object).unwrap();
    let (worker, entered, release) = blocked_runtime_new_file(
        handle.clone(),
        info.root_id,
        "New.md",
        StorageStage::BeforeExternalObjectVerify,
    );
    tokio::time::timeout(Duration::from_secs(10), entered)
        .await
        .unwrap()
        .unwrap();
    worker.abort();
    release.send(()).unwrap();
    assert!(
        await_runtime_not_ready(&handle).await,
        "missing referenced object must be diagnosed even after cancellation"
    );
    assert_eq!(
        handle
            .server_info(rig.paired.device_token.clone())
            .await
            .unwrap()
            .head_seq,
        info.head_seq
    );
    assert_eq!(fs::read(path).unwrap(), b"original document\n");
    assert!(!object.exists());
    assert_eq!(
        fs::read(rig.dir.path().join("notes/Document.md")).unwrap(),
        b"original document\n"
    );
    rig.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancelled_new_file_preparation_failure_still_diagnoses_missing_referenced_object() {
    use std::time::Duration;
    let rig = RuntimeRig::new().await;
    let handle = rig.runtime.handle();
    let info = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap();
    let source = rig.dir.path().join("notes/New.md");
    let original = rig.dir.path().join("notes/Document.md");
    let bytes = b"original document\n";
    fs::write(&source, bytes).unwrap();
    let object = rig
        .dir
        .path()
        .join("state/objects/sha256")
        .join(&rig.document.content_hash);
    fs::remove_file(&object).unwrap();
    let mut notices = handle.event_subscription();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let worker_handle = handle.clone();
    let worker = tokio::spawn(async move {
        let mut entered = Some(entered_tx);
        worker_handle
            .test_reconcile_new_file(info.root_id, "New.md".into(), move |stage| {
                if stage == StorageStage::BeforeObjectFileSync {
                    let _ = entered.take().unwrap().send(());
                    release_rx
                        .recv_timeout(Duration::from_secs(20))
                        .map_err(|_| Error::Storage)?;
                    return Err(Error::Storage);
                }
                Ok(())
            })
            .await
    });
    tokio::time::timeout(Duration::from_secs(10), entered_rx)
        .await
        .unwrap()
        .unwrap();
    worker.abort();
    release_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while handle.test_observation_active() != 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let ready = handle.health().await.unwrap().ready;
    let head = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let source_bytes = fs::read(source).unwrap();
    let original_bytes = fs::read(original).unwrap();
    let missing = !object.exists();
    let no_notice = matches!(
        notices.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    );
    rig.runtime.shutdown().await.unwrap();
    assert!(
        !ready,
        "missing referenced object must be diagnosed despite later preparation failure and cancellation"
    );
    assert_eq!(head, info.head_seq);
    assert_eq!(source_bytes, bytes);
    assert_eq!(original_bytes, bytes);
    assert!(missing);
    assert!(no_notice);
}

#[tokio::test]
async fn cancelled_new_file_diagnoses_corrupt_referenced_object_published_after_missing_observation()
 {
    use std::time::Duration;
    let rig = RuntimeRig::new().await;
    let handle = rig.runtime.handle();
    let info = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap();
    let source = rig.dir.path().join("notes/New.md");
    let original = rig.dir.path().join("notes/Document.md");
    let bytes = b"original document\n";
    let corrupt = b"corrupt document!\n";
    assert_eq!(bytes.len(), corrupt.len());
    fs::write(&source, bytes).unwrap();
    let object = rig
        .dir
        .path()
        .join("state/objects/sha256")
        .join(&rig.document.content_hash);
    fs::remove_file(&object).unwrap();
    let mut notices = handle.event_subscription();
    let (worker, entered, release) = blocked_runtime_new_file(
        handle.clone(),
        info.root_id,
        "New.md",
        StorageStage::BeforeObjectFileSync,
    );
    tokio::time::timeout(Duration::from_secs(10), entered)
        .await
        .unwrap()
        .unwrap();
    worker.abort();
    fs::write(&object, corrupt).unwrap();
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while handle.test_observation_active() != 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let ready = handle.health().await.unwrap().ready;
    let head = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let source_bytes = fs::read(source).unwrap();
    let original_bytes = fs::read(original).unwrap();
    let object_bytes = fs::read(object).unwrap();
    let no_notice = matches!(
        notices.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    );
    rig.runtime.shutdown().await.unwrap();
    assert!(
        !ready,
        "a corrupt referenced object published during the cancelled observation must be diagnosed"
    );
    assert_eq!(head, info.head_seq);
    assert_eq!(source_bytes, bytes);
    assert_eq!(original_bytes, bytes);
    assert_eq!(object_bytes, corrupt);
    assert!(no_notice);
}

#[tokio::test]
async fn cancelled_existing_file_preparation_failure_still_diagnoses_missing_referenced_object() {
    use std::time::Duration;
    let rig = RuntimeRig::new().await;
    let handle = rig.runtime.handle();
    let info = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap();
    let source = rig.dir.path().join("notes/Document.md");
    let other = rig.dir.path().join("notes/Other.md");
    let bytes = b"other original\n";
    fs::write(&source, bytes).unwrap();
    let object = rig
        .dir
        .path()
        .join("state/objects/sha256")
        .join(&rig.other.content_hash);
    let original_object = rig
        .dir
        .path()
        .join("state/objects/sha256")
        .join(&rig.document.content_hash);
    fs::remove_file(&object).unwrap();
    let mut notices = handle.event_subscription();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let worker_handle = handle.clone();
    let document_id = rig.document.node_id;
    let worker = tokio::spawn(async move {
        let mut entered = Some(entered_tx);
        worker_handle
            .test_reconcile_file(document_id, move |stage| {
                if stage == StorageStage::BeforeObjectFileSync {
                    let _ = entered.take().unwrap().send(());
                    release_rx
                        .recv_timeout(Duration::from_secs(20))
                        .map_err(|_| Error::Storage)?;
                    return Err(Error::Storage);
                }
                Ok(())
            })
            .await
    });
    tokio::time::timeout(Duration::from_secs(10), entered_rx)
        .await
        .unwrap()
        .unwrap();
    worker.abort();
    release_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while handle.test_observation_active() != 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let ready = handle.health().await.unwrap().ready;
    let head = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let source_bytes = fs::read(source).unwrap();
    let other_bytes = fs::read(other).unwrap();
    let original_object_bytes = fs::read(original_object).unwrap();
    let missing = !object.exists();
    let no_notice = matches!(
        notices.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    );
    rig.runtime.shutdown().await.unwrap();
    assert!(
        !ready,
        "missing referenced object must be diagnosed despite later preparation failure and cancellation"
    );
    assert_eq!(head, info.head_seq);
    assert_eq!(source_bytes, bytes);
    assert_eq!(other_bytes, bytes);
    assert_eq!(original_object_bytes, b"original document\n");
    assert!(missing);
    assert!(no_notice);
}

#[tokio::test]
async fn unsafe_unreferenced_new_file_object_does_not_block_workspace() {
    use crate::runtime::RuntimeError;
    let rig = RuntimeRig::new().await;
    let handle = rig.runtime.handle();
    let info = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap();
    let bytes = b"unreferenced new source\n";
    let path = rig.dir.path().join("notes/New.md");
    fs::write(&path, bytes).unwrap();
    let object = rig
        .dir
        .path()
        .join("state/objects/sha256")
        .join(digest(bytes));
    let linked = rig.dir.path().join("unreferenced-hardlink-copy");
    fs::write(&object, bytes).unwrap();
    fs::hard_link(&object, &linked).unwrap();
    assert_eq!(
        handle
            .reconcile_new_file(info.root_id, "New.md".into())
            .await,
        Err(RuntimeError::Workspace(Error::Storage))
    );
    assert!(handle.health().await.unwrap().ready);
    assert_eq!(
        handle
            .server_info(rig.paired.device_token.clone())
            .await
            .unwrap()
            .head_seq,
        info.head_seq
    );
    assert_eq!(fs::read(path).unwrap(), bytes);
    assert_eq!(fs::read(object).unwrap(), bytes);
    assert_eq!(fs::read(linked).unwrap(), bytes);
    rig.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn new_file_candidate_retries_if_api_publishes_same_object_first() {
    use crate::runtime::RuntimeError;
    use std::time::Duration;
    let rig = RuntimeRig::new().await;
    let handle = rig.runtime.handle();
    let info = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap();
    let bytes = b"shared bytes from different sources\n";
    let path = rig.dir.path().join("notes/New.md");
    fs::write(&path, bytes).unwrap();
    let (worker, entered, release) = blocked_runtime_new_file(
        handle.clone(),
        info.root_id,
        "New.md",
        StorageStage::BeforeRecoveryFileSync,
    );
    tokio::time::timeout(Duration::from_secs(10), entered)
        .await
        .unwrap()
        .unwrap();
    let save = SaveContent {
        epoch: rig.paired.workspace_epoch,
        stream_id: rig.paired.device_stream_id.parse().unwrap(),
        operation_seq: 1,
        operation_id: op_id(),
        node_id: rig.other.node_id,
        expected_content_rev: rig.other.content_rev.clone(),
        content_hash: digest(bytes),
        bytes: bytes.to_vec(),
    };
    assert!(matches!(
        handle
            .save_content(rig.paired.device_token.clone(), save)
            .await
            .unwrap()
            .outcome,
        Outcome::Saved(_)
    ));
    let head = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    release.send(()).unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(10), worker)
            .await
            .unwrap()
            .unwrap(),
        Err(RuntimeError::Workspace(Error::ExternalChange))
    );
    assert!(handle.health().await.unwrap().ready);
    assert_eq!(
        handle
            .server_info(rig.paired.device_token.clone())
            .await
            .unwrap()
            .head_seq,
        head
    );
    let created = handle
        .reconcile_new_file(info.root_id, "New.md".into())
        .await
        .unwrap();
    assert_eq!(
        handle
            .content(rig.paired.device_token.clone(), created.node_id, None)
            .await
            .unwrap()
            .1,
        bytes
    );
    assert_eq!(fs::read(path).unwrap(), bytes);
    rig.runtime.shutdown().await.unwrap();
}

async fn recheck_preserves_original_preparation_error(failing_stage: StorageStage) {
    use crate::runtime::RuntimeError;
    use std::time::Duration;
    let rig = RuntimeRig::new().await;
    let handle = rig.runtime.handle();
    let info = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap();
    let source = rig.dir.path().join("notes/New.md");
    let bytes = b"original document\n";
    fs::write(&source, bytes).unwrap();
    let object = rig
        .dir
        .path()
        .join("state/objects/sha256")
        .join(&rig.document.content_hash);
    fs::remove_file(&object).unwrap();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let worker_handle = handle.clone();
    let worker = tokio::spawn(async move {
        let mut entered = Some(entered_tx);
        worker_handle
            .test_reconcile_new_file(info.root_id, "New.md".into(), move |stage| {
                if stage == failing_stage {
                    let _ = entered.take().unwrap().send(());
                    release_rx
                        .recv_timeout(Duration::from_secs(20))
                        .map_err(|_| Error::Storage)?;
                    return Err(Error::Storage);
                }
                Ok(())
            })
            .await
    });
    tokio::time::timeout(Duration::from_secs(10), entered_rx)
        .await
        .unwrap()
        .unwrap();
    let save = SaveContent {
        epoch: rig.paired.workspace_epoch,
        stream_id: rig.paired.device_stream_id.parse().unwrap(),
        operation_seq: 1,
        operation_id: op_id(),
        node_id: rig.other.node_id,
        expected_content_rev: rig.other.content_rev.clone(),
        content_hash: digest(bytes),
        bytes: bytes.to_vec(),
    };
    assert!(matches!(
        handle
            .save_content(rig.paired.device_token.clone(), save)
            .await
            .unwrap()
            .outcome,
        Outcome::Saved(_)
    ));
    let head = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    release_tx.send(()).unwrap();
    let result = tokio::time::timeout(Duration::from_secs(10), worker)
        .await
        .unwrap()
        .unwrap();
    let ready = handle.health().await.unwrap().ready;
    let final_head = handle
        .server_info(rig.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let source_bytes = fs::read(source).unwrap();
    let object_bytes = fs::read(object).unwrap();
    rig.runtime.shutdown().await.unwrap();
    assert_eq!(result, Err(RuntimeError::Workspace(Error::Storage)));
    assert!(ready);
    assert_eq!(final_head, head);
    assert_eq!(source_bytes, bytes);
    assert_eq!(object_bytes, bytes);
}

#[tokio::test]
async fn recheck_preserves_proposed_object_preparation_error_after_api_publication() {
    recheck_preserves_original_preparation_error(StorageStage::BeforeObjectFileSync).await;
}

#[tokio::test]
async fn recheck_preserves_source_preparation_error_after_api_publication() {
    recheck_preserves_original_preparation_error(StorageStage::BeforeRecoveryFileSync).await;
}
