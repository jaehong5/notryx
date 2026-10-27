use notryx_client_runtime::{AbandonMutation, ContentMutation, Error, OperationLookup, Session};
use notryx_protocol::{AdminRequest, AdminResponse, AdminResult, OperationReceipt};
use notryx_server::{
    Workspace, admin,
    config::{Config, ServerConfig, WorkspaceConfig},
    http::RunningServer,
};
use std::os::unix::fs::PermissionsExt;

#[tokio::test]
async fn exact_retry_receipt_lookup_conflict_and_ack_use_the_real_server() {
    let directory = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    let notes = directory.path().join("notes");
    let state = directory.path().join("state");
    std::fs::create_dir(&notes).unwrap();
    std::fs::create_dir(&state).unwrap();
    let mut workspace = Workspace::initialize(&notes, &state).await.unwrap();
    std::fs::write(notes.join("Example.md"), b"# original\n").unwrap();
    let document = workspace.adopt_root_document("Example.md").await.unwrap();
    workspace.close().await.unwrap();
    let config = Config {
        server: ServerConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            admin_socket: directory.path().join("admin.sock"),
        },
        workspace: WorkspaceConfig {
            notes_root: notes.clone(),
            state_root: state,
            uploads: Default::default(),
        },
    };
    let server = RunningServer::start(config.clone()).await.unwrap();
    let AdminResponse::Ok {
        result:
            AdminResult::Status {
                workspace_id,
                owner_principal_id,
                ..
            },
    } = admin::request(&config.server.admin_socket, &AdminRequest::Status {})
        .await
        .unwrap()
    else {
        panic!("admin status")
    };
    let AdminResponse::Ok {
        result: AdminResult::Pairing { pairing_secret, .. },
    } = admin::request(
        &config.server.admin_socket,
        &AdminRequest::IssuePairing {
            principal_id: owner_principal_id,
        },
    )
    .await
    .unwrap()
    else {
        panic!("pairing grant")
    };
    let session = Session::pair(
        &format!("http://{}", server.local_addr()),
        workspace_id,
        &pairing_secret,
    )
    .await
    .unwrap();
    let node = session
        .inventory(None)
        .await
        .unwrap()
        .nodes
        .into_iter()
        .find(|node| node.node_id == document.node_id)
        .unwrap();
    let mutation = ContentMutation {
        binding: session.binding().clone(),
        operation_id: uuid::Uuid::now_v7().to_string().parse().unwrap(),
        operation_seq: 1,
        node_id: document.node_id,
        expected_revision: node.content_rev.unwrap(),
        bytes: "# 변경\n".as_bytes().to_vec(),
    };
    assert!(matches!(
        session.operation(&mutation).await.unwrap(),
        OperationLookup::Missing
    ));
    let receipt = session.put_content(&mutation).await.expect("commit");
    assert!(matches!(receipt, OperationReceipt::Committed { .. }));
    assert_eq!(session.put_content(&mutation).await.unwrap(), receipt);
    assert_eq!(
        session.operation(&mutation).await.unwrap(),
        OperationLookup::Receipt(Box::new(receipt))
    );
    let changed_retry = ContentMutation {
        bytes: b"different retry".to_vec(),
        ..mutation.clone()
    };
    assert_eq!(
        session.put_content(&changed_retry).await,
        Err(Error::IdempotencyMismatch)
    );
    assert_eq!(
        std::fs::read(notes.join("Example.md")).unwrap(),
        mutation.bytes
    );
    let already_committed = AbandonMutation {
        binding: mutation.binding.clone(),
        operation_id: mutation.operation_id,
        operation_seq: mutation.operation_seq,
        node_id: mutation.node_id,
        bytes: mutation.bytes.clone(),
    };
    assert!(matches!(
        session.abandon(&already_committed).await.unwrap(),
        OperationReceipt::Committed { .. }
    ));
    let mut mismatched_payload = already_committed.clone();
    mismatched_payload.bytes = b"different".to_vec();
    assert_eq!(
        session.abandon(&mismatched_payload).await,
        Err(Error::Contract)
    );
    let conflict = ContentMutation {
        operation_id: uuid::Uuid::now_v7().to_string().parse().unwrap(),
        operation_seq: 2,
        bytes: b"another edit".to_vec(),
        ..mutation.clone()
    };
    assert!(matches!(
        session.put_content(&conflict).await.unwrap(),
        OperationReceipt::PreconditionFailed { .. }
    ));
    assert_eq!(
        std::fs::read(notes.join("Example.md")).unwrap(),
        mutation.bytes
    );
    session
        .acknowledge(2)
        .await
        .expect("acknowledges terminal receipts");
    assert_eq!(
        session.operation(&mutation).await.unwrap(),
        OperationLookup::Retired
    );
    let abandon = AbandonMutation {
        binding: session.binding().clone(),
        operation_id: uuid::Uuid::now_v7().to_string().parse().unwrap(),
        operation_seq: 3,
        node_id: document.node_id,
        bytes: b"never sent".to_vec(),
    };
    let abandoned = session.abandon(&abandon).await.unwrap();
    assert!(matches!(abandoned, OperationReceipt::Abandoned { .. }));
    assert_eq!(session.abandon(&abandon).await.unwrap(), abandoned);
    assert_eq!(
        std::fs::read(notes.join("Example.md")).unwrap(),
        "# 변경\n".as_bytes()
    );
    let mut wrong_abandon = abandon.clone();
    wrong_abandon.binding.device_stream_id = uuid::Uuid::new_v4();
    assert_eq!(session.abandon(&wrong_abandon).await, Err(Error::Binding));
    session.acknowledge(3).await.unwrap();
    let mut wrong_binding = conflict.clone();
    wrong_binding.binding.device_stream_id = uuid::Uuid::new_v4();
    assert_eq!(
        session.put_content(&wrong_binding).await,
        Err(Error::Binding)
    );
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn forged_success_receipt_with_wrong_payload_hash_is_not_accepted() {
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let workspace_id = uuid::Uuid::new_v4().to_string();
    let epoch = uuid::Uuid::new_v4().to_string();
    let operation_id = uuid::Uuid::now_v7().to_string();
    let node_id = uuid::Uuid::now_v7().to_string();
    let pair = serde_json::json!({
        "protocol": "mdw-sync/1.2", "workspace_id": workspace_id, "workspace_epoch": epoch,
        "principal_id": uuid::Uuid::new_v4().to_string(), "device_id": uuid::Uuid::new_v4().to_string(),
        "device_stream_id": uuid::Uuid::new_v4().to_string(), "device_token": "synthetic-device-token",
        "role": "owner", "idle_expires_at": "1", "absolute_expires_at": "2"
    });
    let receipt = serde_json::json!({
        "status": "committed", "operation_id": operation_id, "operation_seq": "1", "workspace_epoch": epoch,
        "node_id": node_id, "content_rev": "new-rev", "content_etag": "\"new-rev\"",
        "meta_rev": "meta-rev", "meta_etag": "\"meta-rev\"", "node_etag": "\"node-etag\"",
        "content_hash": format!("sha256:{}", "0".repeat(64)), "commit_seq": "1"
    });
    let server = tokio::spawn(async move {
        for body in [pair.to_string(), receipt.to_string()] {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = vec![0; 4096];
            let _ = stream.read(&mut bytes).await.unwrap();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        }
    });
    let session = Session::pair(&origin, workspace_id.parse().unwrap(), "synthetic-secret")
        .await
        .unwrap();
    let request = ContentMutation {
        binding: session.binding().clone(),
        operation_id: operation_id.parse().unwrap(),
        operation_seq: 1,
        node_id: node_id.parse().unwrap(),
        expected_revision: notryx_domain::ContentRevision::new("old-rev").unwrap(),
        bytes: b"different content".to_vec(),
    };
    assert_eq!(session.put_content(&request).await, Err(Error::Contract));
    server.await.unwrap();
}

#[tokio::test]
async fn ack_cannot_retire_beyond_the_durable_requested_watermark() {
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let workspace_id = uuid::Uuid::new_v4().to_string();
    let epoch = uuid::Uuid::new_v4().to_string();
    let stream = uuid::Uuid::new_v4().to_string();
    let pair = serde_json::json!({
        "protocol": "mdw-sync/1.2", "workspace_id": workspace_id, "workspace_epoch": epoch,
        "principal_id": uuid::Uuid::new_v4().to_string(), "device_id": uuid::Uuid::new_v4().to_string(),
        "device_stream_id": stream, "device_token": "synthetic-device-token", "role": "owner",
        "idle_expires_at": "1", "absolute_expires_at": "2"
    });
    let ack = serde_json::json!({
        "workspace_epoch": epoch, "device_stream_id": stream, "last_seq": "2", "retired_through": "2"
    });
    let server = tokio::spawn(async move {
        for body in [pair.to_string(), ack.to_string()] {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = vec![0; 4096];
            let _ = stream.read(&mut bytes).await.unwrap();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        }
    });
    let session = Session::pair(&origin, workspace_id.parse().unwrap(), "synthetic-secret")
        .await
        .unwrap();
    assert_eq!(session.acknowledge(1).await, Err(Error::Binding));
    server.await.unwrap();
}
