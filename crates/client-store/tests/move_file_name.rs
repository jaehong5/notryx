use notryx_client_store::{ClientStore, RegistrationBinding};
use notryx_protocol::MoveNodeRequest;
use serde_json::json;
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::path::Path;
use uuid::Uuid;

const ROOT: &str = "019a1000-0000-7000-8000-000000000001";
const TARGET: &str = "019a1000-0000-7000-8000-000000000002";
const DESTINATION: &str = "019a1000-0000-7000-8000-000000000003";
const META_ETAG: &str = "\"meta-1\"";

fn binding() -> RegistrationBinding {
    RegistrationBinding {
        server_origin: "https://workspace.example".into(),
        workspace_id: Uuid::new_v4().to_string(),
        workspace_epoch: Uuid::new_v4().to_string(),
        device_id: Uuid::new_v4().to_string(),
        device_stream_id: Uuid::new_v4().to_string(),
    }
}

fn move_request() -> MoveNodeRequest {
    MoveNodeRequest {
        expected_parent_id: ROOT.parse().unwrap(),
        parent_id: DESTINATION.parse().unwrap(),
        name: "renamed.md".into(),
    }
}

async fn database(path: &Path) -> SqliteConnection {
    SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(path)
            .foreign_keys(true),
    )
    .await
    .unwrap()
}

async fn fixture(path: &Path, registration: &RegistrationBinding) {
    let mut store = ClientStore::open(path).await.unwrap();
    store.activate_registration(registration).await.unwrap();
    store.close().await.unwrap();
    let root = json!({"node_id":ROOT,"parent_id":null,"name":"","kind":"folder","deleted":false,"content_rev":null,"content_etag":null,"meta_rev":"root-meta","meta_etag":"\"root-meta\"","node_etag":"\"root-node\"","content_hash":null,"size":null,"children_generation":"2","last_change_seq":"1"});
    let target = json!({"node_id":TARGET,"parent_id":ROOT,"name":"original.md","kind":"file","deleted":false,"content_rev":"content-1","content_etag":"\"content-1\"","meta_rev":"meta-1","meta_etag":META_ETAG,"node_etag":"\"node-1\"","content_hash":"sha256:f34848ca92665c342abd5816c9e3eda0e82180671195362bcd0080544a3bc2ac","size":"5","children_generation":"0","last_change_seq":"1"});
    let destination = json!({"node_id":DESTINATION,"parent_id":ROOT,"name":"destination","kind":"folder","deleted":false,"content_rev":null,"content_etag":null,"meta_rev":"destination-meta","meta_etag":"\"destination-meta\"","node_etag":"\"destination-node\"","content_hash":null,"size":null,"children_generation":"0","last_change_seq":"1"});
    let mut db = database(path).await;
    sqlx::query("INSERT INTO remote_index (registration_id,root_id,cursor) VALUES (1,?,'1')")
        .bind(ROOT)
        .execute(&mut db)
        .await
        .unwrap();
    for (id, node) in [(ROOT, root), (TARGET, target), (DESTINATION, destination)] {
        sqlx::query("INSERT INTO remote_nodes (registration_id,node_id,state,content_ready) VALUES (1,?,?,0)")
            .bind(id).bind(node.to_string()).execute(&mut db).await.unwrap();
    }
    db.close().await.unwrap();
}

async fn remote_snapshot(path: &Path) -> (Vec<(String, String, i64)>, String) {
    let mut db = database(path).await;
    let nodes = sqlx::query_as("SELECT node_id,state,content_ready FROM remote_nodes WHERE registration_id=1 ORDER BY node_id").fetch_all(&mut db).await.unwrap();
    let cursor = sqlx::query_scalar("SELECT cursor FROM remote_index WHERE registration_id=1")
        .fetch_one(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    (nodes, cursor)
}

#[tokio::test]
async fn move_requires_markdown_destination_without_consuming_sequence() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite3");
    let registration = binding();
    fixture(&path, &registration).await;
    let tree = remote_snapshot(&path).await;
    let mut store = ClientStore::open(&path).await.unwrap();
    for name in ["renamed", "renamed.txt", "renamed.md.bak"] {
        let request = MoveNodeRequest {
            name: name.into(),
            ..move_request()
        };
        assert!(
            store
                .queue_move(&registration, TARGET.parse().unwrap(), META_ETAG, &request)
                .await
                .is_err()
        );
        assert!(
            store
                .next_pending_outbox(&registration)
                .await
                .unwrap()
                .is_none()
        );
    }
    let request = MoveNodeRequest {
        name: "Renamed.MD".into(),
        ..move_request()
    };
    let item = store
        .queue_move(&registration, TARGET.parse().unwrap(), META_ETAG, &request)
        .await
        .unwrap();
    assert_eq!(item.operation_seq, 1);
    assert_eq!(item.request.name, "Renamed.MD");
    store.close().await.unwrap();
    assert_eq!(remote_snapshot(&path).await, tree);
}
