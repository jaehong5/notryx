use notryx_client_runtime::{Binding, Error, Session, StoredCredential};
use notryx_protocol::{AdminRequest, AdminResponse, AdminResult};
use notryx_server::{
    Workspace, admin,
    config::{Config, ServerConfig, WorkspaceConfig},
    http::RunningServer,
};
use std::os::unix::fs::PermissionsExt;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use uuid::Uuid;

async fn pair(
    config: &Config,
    address: std::net::SocketAddr,
    workspace: notryx_domain::WorkspaceId,
    owner: &str,
) -> Session {
    let AdminResponse::Ok {
        result: AdminResult::Pairing { pairing_secret, .. },
    } = admin::request(
        &config.server.admin_socket,
        &AdminRequest::IssuePairing {
            principal_id: owner.into(),
        },
    )
    .await
    .unwrap()
    else {
        panic!("pairing")
    };
    Session::pair(&format!("http://{address}"), workspace, &pairing_secret)
        .await
        .unwrap()
}

fn credential_with_binding(credential: &StoredCredential, binding: &Binding) -> StoredCredential {
    let mut value = serde_json::to_value(credential).unwrap();
    value["binding"] = serde_json::to_value(binding).unwrap();
    serde_json::from_value(value).unwrap()
}

#[tokio::test]
async fn restored_binding_rejects_other_device_or_stream_on_real_server() {
    let directory = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    let notes = directory.path().join("notes");
    let state = directory.path().join("state");
    std::fs::create_dir(&notes).unwrap();
    std::fs::create_dir(&state).unwrap();
    Workspace::initialize(&notes, &state)
        .await
        .unwrap()
        .close()
        .await
        .unwrap();
    let config = Config {
        sync: Default::default(),
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
        panic!("status")
    };
    let first = pair(
        &config,
        server.local_addr(),
        workspace_id,
        &owner_principal_id,
    )
    .await;
    let second = pair(
        &config,
        server.local_addr(),
        workspace_id,
        &owner_principal_id,
    )
    .await;
    assert_ne!(first.binding().device_id, second.binding().device_id);
    assert_ne!(
        first.binding().device_stream_id,
        second.binding().device_stream_id
    );
    first.verify_binding().await.unwrap();
    second.verify_binding().await.unwrap();

    let first_credential = first.stored_credential().unwrap();
    let second_credential = second.stored_credential().unwrap();
    let mut wrong_device = first.binding().clone();
    wrong_device.device_id = second.binding().device_id;
    let mut wrong_stream = first.binding().clone();
    wrong_stream.device_stream_id = second.binding().device_stream_id;
    let device_session = Session::from_stored_credential(
        &wrong_device,
        &credential_with_binding(&first_credential, &wrong_device),
    )
    .unwrap();
    let stream_session = Session::from_stored_credential(
        &wrong_stream,
        &credential_with_binding(&first_credential, &wrong_stream),
    )
    .unwrap();
    let mixed_session = Session::from_stored_credential(
        first.binding(),
        &credential_with_binding(&second_credential, first.binding()),
    )
    .unwrap();
    assert!(
        mixed_session.server_info().await.is_ok(),
        "server-info alone does not identify the bearer device"
    );
    let failures = [
        device_session.verify_binding().await,
        stream_session.verify_binding().await,
        mixed_session.verify_binding().await,
    ];
    assert_eq!(
        failures,
        [
            Err(Error::Binding),
            Err(Error::Binding),
            Err(Error::Binding)
        ]
    );
    let result = admin::request(
        &config.server.admin_socket,
        &AdminRequest::RevokeDevice {
            device_id: first.binding().device_id.to_string(),
        },
    )
    .await
    .unwrap();
    assert!(matches!(result, AdminResponse::Ok { .. }));
    assert_eq!(first.verify_binding().await, Err(Error::Unauthorized));
    assert!(second.verify_binding().await.is_ok());
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn restored_binding_rejects_legacy_server_without_identity_endpoint() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let workspace = Uuid::new_v4();
    let epoch = Uuid::new_v4();
    let binding: Binding = serde_json::from_value(serde_json::json!({
        "origin": origin, "workspace_id": workspace.to_string(), "workspace_epoch": epoch.to_string(),
        "device_id": Uuid::new_v4(), "device_stream_id": Uuid::new_v4()
    })).unwrap();
    let credential: StoredCredential = serde_json::from_value(serde_json::json!({
        "binding": binding, "token": "synthetic-only-token"
    }))
    .unwrap();
    let session = Session::from_stored_credential(credential.binding(), &credential).unwrap();
    let info = serde_json::json!({
        "protocol": "mdw-sync/1.2", "workspace_id": workspace.to_string(), "workspace_epoch": epoch.to_string(),
        "root_id": Uuid::now_v7().to_string(), "head_seq": "0", "journal_floor_seq": "0", "ready": true,
        "max_document_bytes": 5242880, "max_attachment_bytes": 26214400
    }).to_string();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut bytes = [0u8; 4096];
        let n = stream.read(&mut bytes).await.unwrap();
        let is_server_info = bytes[..n].starts_with(b"GET /v1/server-info ");
        let (status, body) = if is_server_info {
            ("200 OK", info)
        } else {
            ("404 Not Found", "{}".into())
        };
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).await.unwrap();
    });
    assert_eq!(
        session.verify_binding().await,
        Err(Error::IdentityUnsupported)
    );
    server.await.unwrap();
}

async fn verify_mock_identity(case: &str) -> Result<(), Error> {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let binding: Binding = serde_json::from_value(serde_json::json!({
        "origin": format!("http://{}", listener.local_addr().unwrap()),
        "workspace_id": Uuid::new_v4().to_string(), "workspace_epoch": Uuid::new_v4().to_string(),
        "device_id": Uuid::new_v4(), "device_stream_id": Uuid::new_v4()
    }))
    .unwrap();
    let credential: StoredCredential = serde_json::from_value(
        serde_json::json!({ "binding": binding, "token": "synthetic-only-token" }),
    )
    .unwrap();
    let session = Session::from_stored_credential(credential.binding(), &credential).unwrap();
    let binding = credential.binding();
    let mut body = serde_json::json!({
        "protocol": "mdw-sync/1.2", "workspace_id": binding.workspace_id,
        "workspace_epoch": binding.workspace_epoch, "device_id": binding.device_id.to_string(),
        "device_stream_id": binding.device_stream_id.to_string()
    });
    let (status, media) = match case {
        "unsupported" => ("404 Not Found", "application/json"),
        "gateway_html" => ("404 Not Found", "text/html"),
        "busy" => ("429 Too Many Requests", "application/json"),
        "unavailable" => ("503 Service Unavailable", "application/json"),
        _ => ("200 OK", "application/json"),
    };
    match case {
        "protocol" => body["protocol"] = "mdw-sync/other".into(),
        "workspace" => body["workspace_id"] = Uuid::new_v4().to_string().into(),
        "epoch" => body["workspace_epoch"] = Uuid::new_v4().to_string().into(),
        "device_uuid" => body["device_id"] = "invalid".into(),
        "stream_uuid" => body["device_stream_id"] = Uuid::now_v7().to_string().into(),
        "missing" => {
            body.as_object_mut().unwrap().remove("device_id");
        }
        "extra" => {
            body["unknown"] = true.into();
        }
        _ => {}
    }
    let body = body.to_string();
    let oversized = case == "oversized";
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0u8; 4096];
        let n = stream.read(&mut request).await.unwrap();
        assert!(request[..n].starts_with(b"GET /v1/devices/self "));
        let length = if oversized {
            1024 * 1024 + 1
        } else {
            body.len()
        };
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {media}\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n"
        );
        stream.write_all(response.as_bytes()).await.unwrap();
        if !oversized {
            let _ = stream.write_all(body.as_bytes()).await;
        }
    });
    let result = session.verify_binding().await;
    server.await.unwrap();
    result
}

#[tokio::test]
async fn self_identity_rejects_malformed_and_mismatched_responses() {
    assert_eq!(verify_mock_identity("ok").await, Ok(()));
    for case in ["protocol", "device_uuid", "stream_uuid", "missing", "extra"] {
        assert_eq!(
            verify_mock_identity(case).await,
            Err(Error::Contract),
            "case={case}"
        );
    }
    for case in ["workspace", "epoch"] {
        assert_eq!(
            verify_mock_identity(case).await,
            Err(Error::Binding),
            "case={case}"
        );
    }
    assert_eq!(
        verify_mock_identity("oversized").await,
        Err(Error::TooLarge)
    );
}

#[tokio::test]
async fn self_identity_distinguishes_unsupported_and_temporary_errors() {
    assert_eq!(
        verify_mock_identity("unsupported").await,
        Err(Error::IdentityUnsupported)
    );
    assert_eq!(verify_mock_identity("busy").await, Err(Error::Unavailable));
    assert_eq!(
        verify_mock_identity("unavailable").await,
        Err(Error::Unavailable)
    );
    assert_eq!(
        verify_mock_identity("gateway_html").await,
        Err(Error::GatewayAuthRequired)
    );
}
