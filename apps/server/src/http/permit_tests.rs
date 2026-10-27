use super::*;
use crate::Workspace;
use crate::config::WorkspaceConfig;
use notryx_protocol::CONTRACT;

#[tokio::test]
async fn cancelled_http_writes_retain_body_permits_until_coordinator_drops_queued_bytes() {
    let directory = tempfile::tempdir().unwrap();
    let config = WorkspaceConfig {
        notes_root: directory.path().join("notes"),
        state_root: directory.path().join("state"),
        uploads: Default::default(),
    };
    std::fs::create_dir(&config.notes_root).unwrap();
    std::fs::create_dir(&config.state_root).unwrap();
    let mut workspace = Workspace::initialize(&config.notes_root, &config.state_root)
        .await
        .unwrap();
    std::fs::write(config.notes_root.join("Queued.md"), b"before").unwrap();
    let document = workspace.adopt_root_document("Queued.md").await.unwrap();
    let epoch = workspace.epoch();
    let principal = workspace.auth_status().await.unwrap().owner_principal_id;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let grant = workspace.issue_pairing(principal, now).await.unwrap();
    let device = workspace
        .pair(
            PairRequest {
                protocol: CONTRACT.into(),
                workspace_id: grant.workspace_id,
                pairing_secret: grant.pairing_secret,
            },
            now,
        )
        .await
        .unwrap();
    workspace.close().await.unwrap();
    let runtime = WorkspaceRuntime::start(config).await.unwrap();
    let handle = runtime.handle();
    let permits = Arc::new(Semaphore::new(2));
    let body = b"after";
    let request = || {
        Request::builder()
            .method("PUT")
            .uri(format!("/v1/nodes/{}/content", document.node_id))
            .header("authorization", format!("Bearer {}", device.device_token))
            .header("x-workspace-epoch", epoch.to_string())
            .header("x-device-stream", &device.device_stream_id)
            .header("x-operation-seq", "1")
            .header("idempotency-key", uuid::Uuid::now_v7().to_string())
            .header("if-match", format!("\"{}\"", document.content_rev.as_str()))
            .header(
                "content-digest",
                format!("sha-256=:{}:", STANDARD.encode(Sha256::digest(body))),
            )
            .body(axum::body::Body::from(body.as_slice()))
            .unwrap()
    };
    let release = handle.test_barrier().await;
    let mut tasks = Vec::new();
    for _ in 0..2 {
        let permit = Arc::new(Arc::clone(&permits).try_acquire_owned().unwrap());
        let mut request = request();
        request.extensions_mut().insert(Arc::clone(&permit));
        let handle = handle.clone();
        let id = document.node_id.to_string();
        tasks.push(tokio::spawn(async move {
            let _middleware_permit = permit;
            write::put_content(State(handle), Path(id), request).await
        }));
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        while handle.pending_commands() != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    for task in tasks {
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
    }
    assert_eq!(
        permits.available_permits(),
        0,
        "queued bodies must retain both admission slots after HTTP cancellation"
    );
    assert!(Arc::clone(&permits).try_acquire_owned().is_err());
    assert_eq!(
        runtime_error(RuntimeError::Busy).status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while permits.available_permits() != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let permit = Arc::new(Arc::clone(&permits).try_acquire_owned().unwrap());
    let mut request = request();
    request.extensions_mut().insert(permit);
    let response =
        write::put_content(State(handle), Path(document.node_id.to_string()), request).await;
    assert_eq!(response.status(), StatusCode::OK);
    runtime.shutdown().await.unwrap();
}
