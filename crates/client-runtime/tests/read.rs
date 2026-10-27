use notryx_client_runtime::{Error, Session};
use notryx_protocol::{AdminRequest, AdminResponse, AdminResult};
use notryx_server::{
    Workspace, admin,
    config::{Config, ServerConfig, WorkspaceConfig},
    http::RunningServer,
};
use std::os::unix::fs::PermissionsExt;

#[tokio::test]
async fn paired_client_reads_inventory_and_exact_committed_bytes() {
    let directory = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    let notes = directory.path().join("notes");
    let state = directory.path().join("state");
    std::fs::create_dir(&notes).unwrap();
    std::fs::create_dir(&state).unwrap();
    let bytes = b"\xef\xbb\xbf# Hello\r\n\r\nSource\n";
    let mut workspace = Workspace::initialize(&notes, &state).await.unwrap();
    std::fs::write(notes.join("Example.md"), bytes).unwrap();
    let document = workspace.adopt_root_document("Example.md").await.unwrap();
    let large_bytes = vec![b'a'; 5 * 1024 * 1024 + 1];
    std::fs::write(notes.join("Large.md"), &large_bytes).unwrap();
    let large = workspace.adopt_root_document("Large.md").await.unwrap();
    let maximum_bytes = vec![b'm'; 25 * 1024 * 1024];
    std::fs::write(notes.join("Maximum.md"), &maximum_bytes).unwrap();
    let maximum = workspace.adopt_root_document("Maximum.md").await.unwrap();
    let mut oversized_bytes = maximum_bytes.clone();
    oversized_bytes.push(b'!');
    let oversized_path = notes.join("Oversized.md");
    std::fs::write(&oversized_path, &oversized_bytes).unwrap();
    assert!(matches!(
        workspace.adopt_root_document("Oversized.md").await,
        Err(notryx_server::Error::InvalidRequest)
    ));
    assert!(
        std::fs::read(&oversized_path).unwrap() == oversized_bytes,
        "rejected oversized document changed during adoption"
    );
    workspace.close().await.unwrap();
    let config = Config {
        server: ServerConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            admin_socket: directory.path().join("admin.sock"),
        },
        workspace: WorkspaceConfig {
            notes_root: notes,
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
    .expect("pairing succeeds");
    let info = session.server_info().await.expect("server info");
    assert_eq!(info.workspace_id, workspace_id);
    let page = session.inventory(None).await.expect("inventory");
    let node = page
        .nodes
        .iter()
        .find(|node| node.node_id == document.node_id)
        .expect("document is present");
    assert_eq!(session.content(node).await.expect("content"), bytes);
    let mut changed = node.clone();
    changed.content_rev = Some(notryx_domain::ContentRevision::new("stale-revision").unwrap());
    changed.content_etag = Some("\"stale-revision\"".into());
    assert_eq!(session.content(&changed).await, Err(Error::Changed));
    let mut oversized = node.clone();
    oversized.size = Some((25 * 1024 * 1024 + 1).to_string());
    assert_eq!(session.content(&oversized).await, Err(Error::TooLarge));
    let mut wrong_hash = node.clone();
    wrong_hash.content_hash = Some(format!("sha256:{}", "0".repeat(64)));
    assert_eq!(session.content(&wrong_hash).await, Err(Error::Contract));
    let large_node = page
        .nodes
        .iter()
        .find(|node| node.node_id == large.node_id)
        .expect("large document is present");
    assert_eq!(
        session
            .content(large_node)
            .await
            .expect("large document can be viewed"),
        large_bytes
    );
    let maximum_node = page
        .nodes
        .iter()
        .find(|node| node.node_id == maximum.node_id)
        .expect("maximum-sized document is present");
    assert!(
        session
            .content(maximum_node)
            .await
            .expect("document at the limit can be viewed")
            == maximum_bytes,
        "document at the limit was not returned intact"
    );
    assert!(!page.nodes.iter().any(|node| node.name == "Oversized.md"));
    server.shutdown().await.unwrap();
    assert!(
        std::fs::read(oversized_path).unwrap() == oversized_bytes,
        "rejected oversized document changed after server use"
    );
}

#[tokio::test]
async fn pairing_rejects_redirect_and_does_not_forward_the_secret() {
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        time::{Duration, timeout},
    };
    let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let target_address = target.local_addr().unwrap();
    let proxy = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = vec![0; 4096];
        let _ = stream.read(&mut request).await.unwrap();
        let response = format!(
            "HTTP/1.1 307 Temporary Redirect\r\nLocation: http://{target_address}/v1/pair\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        stream.write_all(response.as_bytes()).await.unwrap();
    });
    let workspace_id = uuid::Uuid::new_v4().to_string().parse().unwrap();
    assert!(matches!(
        Session::pair(&origin, workspace_id, "synthetic-secret").await,
        Err(Error::GatewayAuthRequired)
    ));
    assert!(
        timeout(Duration::from_millis(250), target.accept())
            .await
            .is_err()
    );
    proxy.await.unwrap();
}

#[tokio::test]
async fn credentials_require_https_or_literal_loopback_and_no_url_secrets() {
    let workspace_id = uuid::Uuid::new_v4().to_string().parse().unwrap();
    for origin in [
        "http://example.com",
        "http://localhost:1234",
        "https://user:password@example.com",
        "https://example.com/path",
        "https://example.com/?token=secret",
        "https://example.com/#fragment",
    ] {
        assert!(matches!(
            Session::pair(origin, workspace_id, "synthetic-secret").await,
            Err(Error::InvalidOrigin)
        ));
    }
}

#[tokio::test]
async fn server_info_from_another_epoch_is_not_accepted() {
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let workspace_id = uuid::Uuid::new_v4().to_string();
    let epoch = uuid::Uuid::new_v4().to_string();
    let pair = serde_json::json!({
        "protocol": "mdw-sync/1.2", "workspace_id": workspace_id,
        "workspace_epoch": epoch, "principal_id": uuid::Uuid::new_v4().to_string(),
        "device_id": uuid::Uuid::new_v4().to_string(), "device_stream_id": uuid::Uuid::new_v4().to_string(),
        "device_token": "synthetic-device-token", "role": "owner", "idle_expires_at": "1", "absolute_expires_at": "2"
    });
    let info = serde_json::json!({
        "protocol": "mdw-sync/1.2", "workspace_id": workspace_id,
        "workspace_epoch": uuid::Uuid::new_v4().to_string(), "root_id": uuid::Uuid::now_v7().to_string(),
        "head_seq": "0", "journal_floor_seq": "0", "ready": true,
        "max_document_bytes": 5242880, "max_attachment_bytes": 52428800
    });
    let server = tokio::spawn(async move {
        for body in [pair.to_string(), info.to_string()] {
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
    assert_eq!(session.server_info().await, Err(Error::Binding));
    server.await.unwrap();
}
