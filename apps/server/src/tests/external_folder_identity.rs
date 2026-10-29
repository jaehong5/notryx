use super::*;
use notryx_protocol::NodeKind;
use notryx_server_storage::FolderIdentity;

async fn parent_fixture() -> (Rig, NodeSnapshot, OperationRecord, FolderIdentity) {
    let mut rig = Rig::new().await;
    let request = CreateNode {
        epoch: rig.workspace.epoch(),
        stream_id: rig.stream,
        operation_seq: 1,
        operation_id: op_id(),
        node_id: node_id(),
        parent_id: rig.workspace.state.root_id,
        name: "Folder".into(),
        kind: NodeKind::Folder,
        upload_id: None,
    };
    let receipt = rig.workspace.create_node(request.clone()).await.unwrap();
    let Outcome::Created(folder) = receipt.outcome else {
        panic!("expected the parent folder to be created");
    };
    let operation = rig
        .workspace
        .state
        .operation(rig.stream, request.operation_id)
        .await
        .unwrap();
    let (_, identity) = rig.workspace.files.open_watch_folder("Folder").unwrap();
    (rig, folder, operation, identity)
}

#[tokio::test]
async fn external_folder_identity_original_candidate_rejects_replaced_parent() {
    let (mut rig, folder, receipt, identity) = parent_fixture().await;
    let notes = rig.dir.path().join("notes");
    let original = notes.join("Folder/External folder");
    fs::create_dir(&original).unwrap();
    fs::write(original.join("Original.md"), b"original candidate\n").unwrap();
    let candidate = rig
        .workspace
        .prepare_watched_external_folder(folder.node_id, "External folder", identity)
        .await
        .unwrap()
        .unwrap()
        .observe_folder()
        .unwrap();
    let before = rig.workspace.state.head_seq().await.unwrap();
    let parent_before = rig.workspace.state.node(folder.node_id).await.unwrap();

    let preserved = rig.dir.path().join("preserved-parent");
    fs::rename(notes.join("Folder"), &preserved).unwrap();
    fs::create_dir(notes.join("Folder")).unwrap();
    fs::create_dir(notes.join("Folder/External folder")).unwrap();
    fs::write(
        notes.join("Folder/External folder/Replacement.md"),
        b"replacement candidate\n",
    )
    .unwrap();
    let (_, replacement_identity) = rig.workspace.files.open_watch_folder("Folder").unwrap();
    assert_ne!(identity, replacement_identity);

    assert_eq!(
        rig.workspace.commit_external_folder(candidate).await,
        Err(Error::ExternalChange)
    );
    assert_eq!(rig.workspace.state.head_seq().await.unwrap(), before);
    assert_eq!(
        rig.workspace.state.node(folder.node_id).await.unwrap(),
        parent_before
    );
    assert!(
        !rig.workspace
            .state
            .child_exists(folder.node_id, "External folder")
            .await
            .unwrap()
    );
    assert!(
        rig.workspace
            .changes(before, None)
            .await
            .unwrap()
            .nodes
            .is_empty()
    );
    assert_eq!(
        rig.workspace
            .state
            .operation(rig.stream, receipt.operation_id)
            .await
            .unwrap(),
        receipt
    );
    assert_eq!(
        fs::read(preserved.join("External folder/Original.md")).unwrap(),
        b"original candidate\n"
    );
    assert_eq!(
        fs::read(notes.join("Folder/External folder/Replacement.md")).unwrap(),
        b"replacement candidate\n"
    );
}

#[tokio::test]
async fn external_folder_identity_fresh_candidate_after_replacement_commits() {
    let (mut rig, folder, receipt, old_identity) = parent_fixture().await;
    let notes = rig.dir.path().join("notes");
    fs::create_dir(notes.join("Folder/External folder")).unwrap();
    fs::write(
        notes.join("Folder/External folder/Original.md"),
        b"original candidate\n",
    )
    .unwrap();
    let stale = rig
        .workspace
        .prepare_watched_external_folder(folder.node_id, "External folder", old_identity)
        .await
        .unwrap()
        .unwrap()
        .observe_folder()
        .unwrap();
    let before = rig.workspace.state.head_seq().await.unwrap();
    let preserved = rig.dir.path().join("preserved-parent");
    fs::rename(notes.join("Folder"), &preserved).unwrap();
    fs::create_dir(notes.join("Folder")).unwrap();
    fs::create_dir(notes.join("Folder/External folder")).unwrap();
    fs::write(
        notes.join("Folder/External folder/Replacement.md"),
        b"replacement candidate\n",
    )
    .unwrap();
    let (_, current_identity) = rig.workspace.files.open_watch_folder("Folder").unwrap();
    assert_ne!(old_identity, current_identity);
    assert_eq!(
        rig.workspace.commit_external_folder(stale).await,
        Err(Error::ExternalChange)
    );
    assert_eq!(rig.workspace.state.head_seq().await.unwrap(), before);

    let fresh = rig
        .workspace
        .prepare_watched_external_folder(folder.node_id, "External folder", current_identity)
        .await
        .unwrap()
        .unwrap()
        .observe_folder()
        .unwrap();
    let adopted = rig.workspace.commit_external_folder(fresh).await.unwrap();
    assert_eq!(adopted.parent_id, Some(folder.node_id));
    assert_eq!(adopted.name, "External folder");
    assert_eq!(adopted.kind, NodeKind::Folder);
    assert_eq!(
        rig.workspace.state.head_seq().await.unwrap().get(),
        before.get() + 1
    );
    let page = rig.workspace.changes(before, None).await.unwrap();
    assert!(
        page.nodes
            .iter()
            .any(|node| node.node_id == adopted.node_id)
    );
    assert_eq!(
        rig.workspace
            .state
            .operation(rig.stream, receipt.operation_id)
            .await
            .unwrap(),
        receipt
    );
    assert_eq!(
        fs::read(preserved.join("External folder/Original.md")).unwrap(),
        b"original candidate\n"
    );
    assert_eq!(
        fs::read(notes.join("Folder/External folder/Replacement.md")).unwrap(),
        b"replacement candidate\n"
    );
}

#[tokio::test]
async fn external_folder_identity_unchanged_parent_commits() {
    let (mut rig, folder, receipt, identity) = parent_fixture().await;
    let path = rig.dir.path().join("notes/Folder/External folder");
    fs::create_dir(&path).unwrap();
    fs::write(path.join("Original.md"), b"original candidate\n").unwrap();
    let before = rig.workspace.state.head_seq().await.unwrap();
    let candidate = rig
        .workspace
        .prepare_watched_external_folder(folder.node_id, "External folder", identity)
        .await
        .unwrap()
        .unwrap()
        .observe_folder()
        .unwrap();
    let adopted = rig
        .workspace
        .commit_external_folder(candidate)
        .await
        .unwrap();
    assert_eq!(adopted.parent_id, Some(folder.node_id));
    assert_eq!(adopted.name, "External folder");
    assert_eq!(adopted.kind, NodeKind::Folder);
    assert_eq!(
        rig.workspace.state.head_seq().await.unwrap().get(),
        before.get() + 1
    );
    assert!(
        rig.workspace
            .changes(before, None)
            .await
            .unwrap()
            .nodes
            .iter()
            .any(|node| node.node_id == adopted.node_id)
    );
    assert_eq!(
        rig.workspace
            .state
            .operation(rig.stream, receipt.operation_id)
            .await
            .unwrap(),
        receipt
    );
    assert_eq!(
        fs::read(path.join("Original.md")).unwrap(),
        b"original candidate\n"
    );
}
