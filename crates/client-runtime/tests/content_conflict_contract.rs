use notryx_client_runtime::{ContentMutation, Error, OperationLookup, Session};
use notryx_protocol::OperationReceipt;
use serde_json::json;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use uuid::Uuid;

async fn exercise_conflict(
    current_revision: &str,
) -> (
    Result<OperationReceipt, Error>,
    Result<OperationLookup, Error>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let workspace_id = Uuid::new_v4().to_string();
    let epoch = Uuid::new_v4().to_string();
    let operation_id = Uuid::now_v7().to_string();
    let node_id = Uuid::now_v7().to_string();
    let pair = json!({
        "protocol":"mdw-sync/1.2", "workspace_id":workspace_id, "workspace_epoch":epoch,
        "principal_id":Uuid::new_v4().to_string(), "device_id":Uuid::new_v4().to_string(),
        "device_stream_id":Uuid::new_v4().to_string(), "device_token":"synthetic-device-token",
        "role":"owner", "idle_expires_at":"1", "absolute_expires_at":"2"
    });
    let receipt = json!({
        "status":"precondition_failed", "operation_id":operation_id, "operation_seq":"1", "workspace_epoch":epoch,
        "current_content_rev":current_revision, "current_content_etag":format!("\"{current_revision}\"")
    });
    let error = json!({"code":"content_revision_mismatch", "message":"synthetic", "request_id":Uuid::now_v7().to_string(), "operation":receipt});
    let replies = [
        ("200 OK", pair.to_string()),
        ("412 Precondition Failed", error.to_string()),
        ("200 OK", receipt.to_string()),
    ];
    let server = tokio::spawn(async move {
        for (status, body) in replies {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut input = vec![0; 4096];
            assert!(stream.read(&mut input).await.unwrap() > 0);
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
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
        expected_revision: notryx_domain::ContentRevision::new("revision-a").unwrap(),
        bytes: b"synthetic local".to_vec(),
    };
    let put = session.put_content(&request).await;
    let lookup = session.operation(&request).await;
    server.await.unwrap();
    (put, lookup)
}

#[tokio::test]
async fn equal_current_revision_is_rejected_in_put_and_receipt_lookup() {
    let (put, lookup) = exercise_conflict("revision-a").await;
    assert_eq!(put, Err(Error::Contract));
    assert_eq!(lookup, Err(Error::Contract));
}

#[tokio::test]
async fn different_current_revision_is_accepted_in_put_and_receipt_lookup() {
    let (put, lookup) = exercise_conflict("revision-b").await;
    let receipt = put.unwrap();
    assert!(matches!(
        receipt,
        OperationReceipt::PreconditionFailed { .. }
    ));
    assert_eq!(lookup.unwrap(), OperationLookup::Receipt(Box::new(receipt)));
}
