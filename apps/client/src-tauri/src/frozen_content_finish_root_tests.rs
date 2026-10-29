include!("frozen_content_inspection_tests.rs");

use crate::frozen_content_native::finish_frozen_content_inspection;
use notryx_protocol::WorkspaceFrozenContentInspectRequest;

#[tokio::test]
async fn frozen_content_finish_rejects_changed_root_even_when_cursor_and_document_match() {
    let rig = rig().await;
    let inspection = inspect_frozen_content_for_renderer(
        &rig.runtime,
        "owner",
        &rig.current,
        &rig.old,
        &rig.sent.operation_id,
    )
    .await
    .unwrap();
    let (cursor, nodes, draft, outbox) = {
        let mut state = rig.runtime.0.lock().await;
        let (cursor, nodes) = state.store.sync_index(&rig.current).await.unwrap().unwrap();
        let draft = state
            .store
            .read_registered_checkpoint(&rig.old, &rig.document)
            .await
            .unwrap()
            .unwrap();
        let outbox = state.store.list_outbox(&rig.old, 0, 100).await.unwrap();
        (cursor, nodes, draft, outbox)
    };
    assert!(cursor.get() > 0);
    let target = nodes
        .iter()
        .find(|node| node.node_id.to_string() == rig.document)
        .unwrap()
        .clone();
    let original_root = nodes
        .iter()
        .find(|node| node.parent_id.is_none())
        .unwrap()
        .clone();
    let new_root_id = uuid::Uuid::now_v7().to_string().parse().unwrap();
    let mut new_root = original_root.clone();
    new_root.node_id = new_root_id;
    let mut moved_root = original_root.clone();
    moved_root.parent_id = Some(new_root_id);
    moved_root.name = "synthetic-former-root".into();
    moved_root.last_change_seq = cursor;

    // Direct SQLite mutation models corruption or a race, not a normal sync event.
    let path = rig
        ._directory
        .path()
        .join("frozen-content-inspection.sqlite");
    let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(path))
        .await
        .unwrap();
    let mut tx = db.begin().await.unwrap();
    let registration_id: i64 =
        sqlx::query_scalar("SELECT id FROM client_registrations WHERE status='active'")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
    sqlx::query("UPDATE remote_nodes SET state=? WHERE registration_id=? AND node_id=?")
        .bind(serde_json::to_string(&moved_root).unwrap())
        .bind(registration_id)
        .bind(original_root.node_id.to_string())
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("INSERT INTO remote_nodes(registration_id,node_id,state) VALUES(?,?,?)")
        .bind(registration_id)
        .bind(new_root_id.to_string())
        .bind(serde_json::to_string(&new_root).unwrap())
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("UPDATE remote_index SET root_id=? WHERE registration_id=?")
        .bind(new_root_id.to_string())
        .bind(registration_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    db.close().await.unwrap();

    let changed_index = {
        let mut state = rig.runtime.0.lock().await;
        state.store.sync_index(&rig.current).await.unwrap().unwrap()
    };
    assert_eq!(changed_index.0, cursor);
    assert_eq!(
        changed_index
            .1
            .iter()
            .find(|node| node.node_id == target.node_id),
        Some(&target)
    );
    assert_eq!(
        changed_index
            .1
            .iter()
            .find(|node| node.parent_id.is_none())
            .unwrap()
            .node_id,
        new_root_id
    );
    let request = WorkspaceFrozenContentInspectRequest {
        renderer_session_id: "owner".into(),
        current_binding: crate::to_contract(rig.current.clone()),
        old_binding: crate::to_contract(rig.old.clone()),
        operation_id: rig.sent.operation_id.clone(),
    };
    let result = finish_frozen_content_inspection(&rig.runtime, request, inspection).await;
    assert!(matches!(result, Err(ref error) if error == "stale_inspection"));
    let mut state = rig.runtime.0.lock().await;
    assert_eq!(
        state.store.sync_index(&rig.current).await.unwrap().unwrap(),
        changed_index
    );
    assert_eq!(
        state
            .store
            .read_registered_checkpoint(&rig.old, &rig.document)
            .await
            .unwrap()
            .unwrap(),
        draft
    );
    assert_eq!(
        state.store.list_outbox(&rig.old, 0, 100).await.unwrap(),
        outbox
    );
    drop(state);
    assert_eq!(
        std::fs::read(rig._directory.path().join("notes/move.md")).unwrap(),
        SENT
    );
    rig.server.shutdown().await.unwrap();
}
