use notryx_client_store::{ClientStore, RegistrationBinding};
use notryx_protocol::{ChangesPage, InventoryPage, NodeState, ServerInfo};
use serde_json::json;
use uuid::Uuid;

const ROOT: &str = "019a1000-0000-7000-8000-000000000001";
const FILE_A: &str = "019a1000-0000-7000-8000-000000000002";
const FILE_B: &str = "019a1000-0000-7000-8000-000000000003";

fn binding() -> RegistrationBinding {
    RegistrationBinding {
        server_origin: "https://workspace.example".into(),
        workspace_id: Uuid::new_v4().to_string(),
        workspace_epoch: Uuid::new_v4().to_string(),
        device_id: Uuid::new_v4().to_string(),
        device_stream_id: Uuid::new_v4().to_string(),
    }
}

fn info(binding: &RegistrationBinding, head: u64, floor: u64) -> ServerInfo {
    serde_json::from_value(json!({
        "protocol": "mdw-sync/1.2",
        "workspace_id": binding.workspace_id,
        "workspace_epoch": binding.workspace_epoch,
        "root_id": ROOT,
        "head_seq": head.to_string(),
        "journal_floor_seq": floor.to_string(),
        "ready": true,
        "max_document_bytes": 5242880,
        "max_attachment_bytes": 26214400
    }))
    .unwrap()
}

fn root(seq: u64) -> NodeState {
    serde_json::from_value(json!({
        "node_id": ROOT, "parent_id": null, "name": "", "kind": "folder",
        "deleted": false, "content_rev": null, "content_etag": null,
        "meta_rev": "root-meta", "meta_etag": "\"root-meta\"", "node_etag": "\"root-node\"",
        "content_hash": null, "size": null,
        "children_generation": seq.to_string(), "last_change_seq": seq.to_string()
    }))
    .unwrap()
}

fn file(id: &str, name: &str, seq: u64, deleted: bool) -> NodeState {
    serde_json::from_value(json!({
        "node_id": id, "parent_id": ROOT, "name": name, "kind": "file", "deleted": deleted,
        "content_rev": format!("content-{seq}"), "content_etag": format!("\"content-{seq}\""),
        "meta_rev": format!("meta-{seq}"), "meta_etag": format!("\"meta-{seq}\""),
        "node_etag": format!("\"node-{seq}\""),
        "content_hash": "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        "size": "0", "children_generation": "0", "last_change_seq": seq.to_string()
    }))
    .unwrap()
}

fn inventory(binding: &RegistrationBinding, nodes: Vec<NodeState>) -> InventoryPage {
    InventoryPage {
        workspace_epoch: info(binding, 0, 0).workspace_epoch,
        nodes,
        next_after_id: None,
    }
}

fn changes(
    binding: &RegistrationBinding,
    from: u64,
    through: u64,
    covered: u64,
    nodes: Vec<NodeState>,
) -> ChangesPage {
    serde_json::from_value(json!({
        "workspace_epoch": binding.workspace_epoch,
        "from_exclusive": from.to_string(), "through_seq": through.to_string(),
        "covered_through": covered.to_string(), "has_more": covered < through,
        "nodes": nodes
    }))
    .unwrap()
}

async fn seeded() -> (tempfile::TempDir, ClientStore, RegistrationBinding) {
    let directory = tempfile::tempdir().unwrap();
    let registration = binding();
    let mut store = ClientStore::open(directory.path().join("drafts.db"))
        .await
        .unwrap();
    store.activate_registration(&registration).await.unwrap();
    let session = store
        .begin_bootstrap(&registration, &info(&registration, 1, 0))
        .await
        .unwrap();
    store
        .bootstrap_inventory(
            &registration,
            &session,
            &inventory(
                &registration,
                vec![
                    root(1),
                    file(FILE_A, "a.md", 1, false),
                    file(FILE_B, "b.md", 1, false),
                ],
            ),
        )
        .await
        .unwrap();
    store
        .bootstrap_finish_inventory(&registration, &session, &info(&registration, 1, 0))
        .await
        .unwrap();
    store
        .bootstrap_publish(&registration, &session, &info(&registration, 1, 0))
        .await
        .unwrap();
    (directory, store, registration)
}

#[tokio::test]
async fn page_applies_complete_groups_in_order_and_preserves_draft() {
    let (directory, mut store, registration) = seeded().await;
    let opened = store
        .open_registered_document(&registration, FILE_A, b"base", "base-rev")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(
            &registration,
            FILE_A,
            &opened.edit_session_id,
            1,
            b"local draft",
        )
        .await
        .unwrap();
    store
        .close_registered_document(&registration, FILE_A, &opened.edit_session_id)
        .await
        .unwrap();
    let operation = store
        .queue_checkpoint(&registration, FILE_A, 1)
        .await
        .unwrap();
    let mut renamed_a = file(FILE_A, "b.md", 2, false);
    let mut renamed_b = file(FILE_B, "a.md", 2, false);
    for node in [&mut renamed_a, &mut renamed_b] {
        node.content_rev = Some("content-1".to_string().try_into().unwrap());
        node.content_etag = Some("\"content-1\"".into());
    }
    let deleted_a = file(FILE_A, "b.md", 3, true);
    let page = changes(
        &registration,
        1,
        3,
        3,
        vec![
            root(2),
            renamed_a,
            renamed_b.clone(),
            root(3),
            deleted_a.clone(),
        ],
    );
    store
        .apply_changes(&registration, &page, notryx_domain::ChangeSeq::new(3))
        .await
        .unwrap();
    let (cursor, nodes) = store.sync_index(&registration).await.unwrap().unwrap();
    assert_eq!(cursor.get(), 3);
    assert_eq!(
        nodes.iter().find(|node| node.node_id.to_string() == FILE_A),
        Some(&deleted_a)
    );
    assert_eq!(
        nodes.iter().find(|node| node.node_id.to_string() == FILE_B),
        Some(&renamed_b)
    );
    let draft = store
        .read_registered_checkpoint(&registration, FILE_A)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"local draft");
    assert_eq!(draft.base_bytes, b"base");
    assert_eq!(
        store
            .read_outbox_operation(&registration, &operation.operation_id)
            .await
            .unwrap()
            .unwrap(),
        operation
    );
    store.close().await.unwrap();
    let mut reopened = ClientStore::open(directory.path().join("drafts.db"))
        .await
        .unwrap();
    assert_eq!(
        reopened
            .sync_index(&registration)
            .await
            .unwrap()
            .unwrap()
            .0
            .get(),
        3
    );
    assert_eq!(
        reopened
            .read_registered_checkpoint(&registration, FILE_A)
            .await
            .unwrap()
            .unwrap()
            .bytes,
        b"local draft"
    );
}

#[tokio::test]
async fn invalid_intermediate_tree_rolls_back_whole_page() {
    let (_directory, mut store, registration) = seeded().await;
    let original = store.sync_index(&registration).await.unwrap();
    let page = changes(
        &registration,
        1,
        3,
        3,
        vec![
            file(FILE_A, "b.md", 2, false),
            file(FILE_B, "a.md", 3, false),
        ],
    );
    assert!(
        store
            .apply_changes(&registration, &page, notryx_domain::ChangeSeq::new(3))
            .await
            .is_err()
    );
    assert_eq!(store.sync_index(&registration).await.unwrap(), original);
}

#[tokio::test]
async fn bad_ranges_duplicates_and_revisions_are_rejected_without_advancing() {
    let (_directory, mut store, registration) = seeded().await;
    let original = store.sync_index(&registration).await.unwrap();
    let valid = changes(&registration, 1, 2, 2, vec![file(FILE_A, "a.md", 2, false)]);
    let mut cases = Vec::new();
    let mut gap = valid.clone();
    gap.nodes[0].last_change_seq = notryx_domain::ChangeSeq::new(3);
    cases.push(gap);
    let mut duplicate = valid.clone();
    duplicate.nodes.push(duplicate.nodes[0].clone());
    cases.push(duplicate);
    let mut wrong_epoch = valid.clone();
    wrong_epoch.workspace_epoch = Uuid::new_v4().to_string().parse().unwrap();
    cases.push(wrong_epoch);
    let mut mismatch = valid.clone();
    mismatch.through_seq = notryx_domain::ChangeSeq::new(3);
    mismatch.has_more = true;
    cases.push(mismatch);
    let mut bad_revision = valid.clone();
    bad_revision.nodes[0].content_rev = Some("content-1".to_string().try_into().unwrap());
    bad_revision.nodes[0].content_etag = Some("\"content-1\"".into());
    bad_revision.nodes[0].content_hash = Some(format!("sha256:{}", "a".repeat(64)));
    cases.push(bad_revision);
    for page in cases {
        assert!(
            store
                .apply_changes(&registration, &page, notryx_domain::ChangeSeq::new(2))
                .await
                .is_err()
        );
        assert_eq!(store.sync_index(&registration).await.unwrap(), original);
    }
}

#[tokio::test]
async fn sqlite_cursor_failure_rolls_back_metadata_and_preserves_outbox() {
    use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
    let (directory, mut store, registration) = seeded().await;
    let opened = store
        .open_registered_document(&registration, FILE_A, b"base", "base-rev")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(
            &registration,
            FILE_A,
            &opened.edit_session_id,
            1,
            b"local draft",
        )
        .await
        .unwrap();
    store
        .close_registered_document(&registration, FILE_A, &opened.edit_session_id)
        .await
        .unwrap();
    let operation = store
        .queue_checkpoint(&registration, FILE_A, 1)
        .await
        .unwrap();
    let original = store.sync_index(&registration).await.unwrap();
    let mut injection = SqliteConnection::connect_with(
        &SqliteConnectOptions::new().filename(directory.path().join("drafts.db")),
    )
    .await
    .unwrap();
    sqlx::query("UPDATE remote_nodes SET content_ready=1 WHERE node_id=?")
        .bind(FILE_A)
        .execute(&mut injection)
        .await
        .unwrap();
    sqlx::query("CREATE TRIGGER fail_catchup BEFORE UPDATE ON remote_index BEGIN SELECT RAISE(ABORT,'synthetic failure'); END").execute(&mut injection).await.unwrap();
    let page = changes(&registration, 1, 2, 2, vec![file(FILE_A, "a.md", 2, false)]);
    assert!(
        store
            .apply_changes(&registration, &page, notryx_domain::ChangeSeq::new(2))
            .await
            .is_err()
    );
    assert_eq!(store.sync_index(&registration).await.unwrap(), original);
    assert_eq!(
        store
            .read_outbox_operation(&registration, &operation.operation_id)
            .await
            .unwrap()
            .unwrap(),
        operation
    );
    assert_eq!(
        store
            .read_registered_checkpoint(&registration, FILE_A)
            .await
            .unwrap()
            .unwrap()
            .bytes,
        b"local draft"
    );
    let ready: i64 = sqlx::query_scalar("SELECT content_ready FROM remote_nodes WHERE node_id=?")
        .bind(FILE_A)
        .fetch_one(&mut injection)
        .await
        .unwrap();
    assert_eq!(ready, 1);
    injection.close().await.unwrap();
}

#[tokio::test]
async fn metadata_change_invalidates_only_stale_content_readiness() {
    use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
    let (directory, mut store, registration) = seeded().await;
    let mut other = SqliteConnection::connect_with(
        &SqliteConnectOptions::new().filename(directory.path().join("drafts.db")),
    )
    .await
    .unwrap();
    sqlx::query("UPDATE remote_nodes SET content_ready=1 WHERE node_id IN (?,?)")
        .bind(FILE_A)
        .bind(FILE_B)
        .execute(&mut other)
        .await
        .unwrap();
    let mut renamed = file(FILE_B, "renamed.md", 2, false);
    renamed.content_rev = Some("content-1".to_string().try_into().unwrap());
    renamed.content_etag = Some("\"content-1\"".into());
    let page = changes(
        &registration,
        1,
        2,
        2,
        vec![root(2), file(FILE_A, "a.md", 2, false), renamed],
    );
    store
        .apply_changes(&registration, &page, notryx_domain::ChangeSeq::new(2))
        .await
        .unwrap();
    let statuses: Vec<(String, i64)> = sqlx::query_as(
        "SELECT node_id,content_ready FROM remote_nodes WHERE node_id IN (?,?) ORDER BY node_id",
    )
    .bind(FILE_A)
    .bind(FILE_B)
    .fetch_all(&mut other)
    .await
    .unwrap();
    assert_eq!(statuses, vec![(FILE_A.into(), 0), (FILE_B.into(), 1)]);
    other.close().await.unwrap();
}

#[tokio::test]
async fn later_invalid_group_rolls_back_earlier_valid_changes() {
    let (_directory, mut store, registration) = seeded().await;
    let original = store.sync_index(&registration).await.unwrap();
    let page = changes(
        &registration,
        1,
        3,
        3,
        vec![
            file(FILE_A, "c.md", 2, false),
            file(FILE_B, "c.md", 3, false),
        ],
    );
    assert!(
        store
            .apply_changes(&registration, &page, notryx_domain::ChangeSeq::new(3))
            .await
            .is_err()
    );
    assert_eq!(store.sync_index(&registration).await.unwrap(), original);
}

#[tokio::test]
async fn late_response_cannot_update_previous_or_replacement_registration() {
    use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
    let (directory, mut store, registration) = seeded().await;
    let mut replacement = registration.clone();
    replacement.device_id = Uuid::new_v4().to_string();
    replacement.device_stream_id = Uuid::new_v4().to_string();
    store.activate_registration(&replacement).await.unwrap();
    let page = changes(&registration, 1, 2, 2, vec![file(FILE_A, "a.md", 2, false)]);
    assert!(
        store
            .apply_changes(&registration, &page, notryx_domain::ChangeSeq::new(2))
            .await
            .is_err()
    );
    assert!(store.sync_index(&replacement).await.unwrap().is_none());
    let mut other = SqliteConnection::connect_with(
        &SqliteConnectOptions::new().filename(directory.path().join("drafts.db")),
    )
    .await
    .unwrap();
    let cursors: Vec<String> = sqlx::query_scalar("SELECT cursor FROM remote_index")
        .fetch_all(&mut other)
        .await
        .unwrap();
    assert_eq!(cursors, vec!["1"]);
    other.close().await.unwrap();
}

#[tokio::test]
async fn fixed_through_is_kept_across_pages_and_wrong_end_is_rejected() {
    let (_directory, mut store, registration) = seeded().await;
    let first = changes(&registration, 1, 3, 2, vec![file(FILE_A, "a.md", 2, false)]);
    store
        .apply_changes(&registration, &first, notryx_domain::ChangeSeq::new(3))
        .await
        .unwrap();
    let before_second = store.sync_index(&registration).await.unwrap();
    assert_eq!(before_second.as_ref().unwrap().0.get(), 2);
    let second = changes(&registration, 2, 3, 3, vec![file(FILE_B, "b.md", 3, false)]);
    assert!(
        store
            .apply_changes(&registration, &second, notryx_domain::ChangeSeq::new(4))
            .await
            .is_err()
    );
    assert_eq!(
        store.sync_index(&registration).await.unwrap(),
        before_second
    );
    store
        .apply_changes(&registration, &second, notryx_domain::ChangeSeq::new(3))
        .await
        .unwrap();
    assert_eq!(
        store
            .sync_index(&registration)
            .await
            .unwrap()
            .unwrap()
            .0
            .get(),
        3
    );
}
