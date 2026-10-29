include!("frozen_content_local_index_failure_tests.rs");

use std::time::Duration as IndexRaceDuration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

async fn read_http_request(stream: &mut tokio::net::TcpStream) -> String {
    let mut request = Vec::new();
    let mut buf = [0_u8; 1024];
    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
        let count = stream.read(&mut buf).await.unwrap();
        assert!(count > 0 && request.len() < 16384);
        request.extend_from_slice(&buf[..count]);
    }
    let first_line = request.split(|byte| *byte == b'\n').next().unwrap();
    String::from_utf8(first_line.to_vec()).unwrap()
}

async fn send_http_json(stream: &mut tokio::net::TcpStream, body: &str) {
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await.unwrap();
    stream.write_all(body.as_bytes()).await.unwrap();
    stream.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn frozen_content_local_index_race_contract_unknown_rechecks_index_before_decision() {
    let mut rig = single_operation_rig().await;
    let info = serde_json::to_string(&rig.session.server_info().await.unwrap()).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let old_origin = rig.current.server_origin.clone();
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new().filename(
            rig._directory
                .path()
                .join("frozen-content-inspection.sqlite"),
        ),
    )
    .await
    .unwrap();
    let update = sqlx::query(
        "UPDATE client_registrations SET server_origin=? WHERE status='active' AND server_origin=?",
    )
    .bind(&origin)
    .bind(&old_origin)
    .execute(&mut db)
    .await
    .unwrap();
    assert_eq!(update.rows_affected(), 1);
    db.close().await.unwrap();
    rig.current.server_origin = origin.clone();
    let credential: notryx_client_runtime::StoredCredential = serde_json::from_value(serde_json::json!({
        "binding": {"origin": origin, "workspace_id":rig.current.workspace_id, "workspace_epoch":rig.current.workspace_epoch,
            "device_id":rig.current.device_id, "device_stream_id":rig.current.device_stream_id},
        "token": "synthetic-test-token"
    })).unwrap();
    let fake_session =
        Arc::new(Session::from_stored_credential(credential.binding(), &credential).unwrap());
    let role =
        serde_json::json!({"protocol":"mdw-sync/1.2", "workspace_id":rig.current.workspace_id,
        "workspace_epoch":rig.current.workspace_epoch,"device_id":rig.current.device_id,
        "device_stream_id":rig.current.device_stream_id,"role":"reader"})
        .to_string();
    {
        let mut state = rig.runtime.0.lock().await;
        state.session = Some(fake_session.clone());
    }
    rig.session = fake_session;
    let (fresh_info_started_tx, fresh_info_started_rx) = oneshot::channel();
    let (release_info_tx, release_info_rx) = oneshot::channel();
    let (response_sent_tx, response_sent_rx) = oneshot::channel();
    let fake_server = tokio::spawn(async move {
        let (mut first, _) = listener.accept().await.unwrap();
        assert!(
            read_http_request(&mut first)
                .await
                .starts_with("GET /v1/server-info ")
        );
        send_http_json(&mut first, &info).await;
        let (mut second, _) = listener.accept().await.unwrap();
        assert!(
            read_http_request(&mut second)
                .await
                .starts_with("GET /v1/devices/self/role ")
        );
        send_http_json(&mut second, &role).await;
        let (mut third, _) = listener.accept().await.unwrap();
        assert!(
            read_http_request(&mut third)
                .await
                .starts_with("GET /v1/server-info ")
        );
        fresh_info_started_tx.send(()).unwrap();
        release_info_rx.await.unwrap();
        send_http_json(&mut third, &info).await;
        response_sent_tx.send(()).unwrap();
    });
    let before = command_sources(&rig).await;
    let operation_evidence = saved_operation_evidence(&rig).await;
    let before_watermarks = stream_watermarks(
        &rig._directory
            .path()
            .join("frozen-content-inspection.sqlite"),
    )
    .await;
    let review = local_review(&rig, &rig.sent.operation_id).await.unwrap();
    assert_eq!(review.remote_evidence, LocalRemoteEvidence::Unknown);
    assert!(!review.inspected.remote_index_error);
    let runtime = Runtime(rig.runtime.0.clone());
    let task =
        tokio::spawn(async move { local_dismiss_for_renderer(&runtime, "owner", review).await });
    tokio::time::timeout(IndexRaceDuration::from_secs(10), fresh_info_started_rx)
        .await
        .unwrap()
        .unwrap();

    let initial_guard = rig.runtime.0.lock().await;
    release_info_tx.send(()).unwrap();
    tokio::time::timeout(IndexRaceDuration::from_secs(10), response_sent_rx)
        .await
        .unwrap()
        .unwrap();
    tokio::time::sleep(IndexRaceDuration::from_millis(150)).await;
    let runtime_lock = rig.runtime.0.clone();
    let db_path = rig
        ._directory
        .path()
        .join("frozen-content-inspection.sqlite");
    let document_id = rig.document.clone();
    let (queued_tx, queued_rx) = oneshot::channel();
    let injector = tokio::spawn(async move {
        use std::{
            future::{Future, poll_fn},
            task::Poll,
        };
        let mut lock = Box::pin(runtime_lock.lock());
        let mut queued_tx = Some(queued_tx);
        let _guard = poll_fn(|cx| match lock.as_mut().poll(cx) {
            Poll::Ready(guard) => Poll::Ready(guard),
            Poll::Pending => {
                if let Some(sender) = queued_tx.take() {
                    sender.send(()).unwrap();
                }
                Poll::Pending
            }
        })
        .await;
        let mut db =
            SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&db_path))
                .await
                .unwrap();
        let result = sqlx::query("UPDATE remote_nodes SET state='synthetic invalid metadata' WHERE registration_id=(SELECT id FROM client_registrations WHERE status='active') AND node_id=?")
            .bind(&document_id).execute(&mut db).await.unwrap();
        assert_eq!(result.rows_affected(), 1);
        db.close().await.unwrap();
    });
    tokio::time::timeout(IndexRaceDuration::from_secs(10), queued_rx)
        .await
        .unwrap()
        .unwrap();
    drop(initial_guard);
    injector.await.unwrap();
    let outcome = tokio::time::timeout(IndexRaceDuration::from_secs(10), task)
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(
            outcome.as_ref().map_err(String::as_str),
            Err("resync_required") | Err("UnsupportedFormat")
        ),
        "index damage must reach the final index check: {outcome:?}"
    );
    assert_eq!(dismiss_status(&rig, "owner").await.unwrap(), None);
    assert!(command_sources(&rig).await == before);
    assert_eq!(saved_operation_evidence(&rig).await, operation_evidence);
    assert_eq!(
        stream_watermarks(
            &rig._directory
                .path()
                .join("frozen-content-inspection.sqlite")
        )
        .await,
        before_watermarks
    );
    fake_server.await.unwrap();
    rig.server.shutdown().await.unwrap();
}
