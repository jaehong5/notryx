use notryx_client_runtime::{Binding, Error, Session, StoredCredential};
use notryx_protocol::{AdminRequest, AdminResponse, AdminResult, WorkspaceRole};
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
    principal: &str,
) -> Session {
    let AdminResponse::Ok {
        result: AdminResult::Pairing { pairing_secret, .. },
    } = admin::request(
        &config.server.admin_socket,
        &AdminRequest::IssuePairing {
            principal_id: principal.into(),
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
async fn current_role_tracks_principal_changes_and_validates_real_device_binding() {
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
    let AdminResponse::Ok {
        result: AdminResult::Principal { principal_id },
    } = admin::request(
        &config.server.admin_socket,
        &AdminRequest::CreatePrincipal {
            role: WorkspaceRole::Editor,
        },
    )
    .await
    .unwrap()
    else {
        panic!("principal")
    };
    let owner = pair(
        &config,
        server.local_addr(),
        workspace_id,
        &owner_principal_id,
    )
    .await;
    let first = pair(&config, server.local_addr(), workspace_id, &principal_id).await;
    let second = pair(&config, server.local_addr(), workspace_id, &principal_id).await;
    assert_ne!(first.binding().device_id, second.binding().device_id);
    assert_ne!(
        first.binding().device_stream_id,
        second.binding().device_stream_id
    );
    assert_eq!(owner.current_role().await, Ok(WorkspaceRole::Owner));
    assert_eq!(first.current_role().await, Ok(WorkspaceRole::Editor));
    assert_eq!(second.current_role().await, Ok(WorkspaceRole::Editor));
    for role in [WorkspaceRole::Reader, WorkspaceRole::Editor] {
        let result = admin::request(
            &config.server.admin_socket,
            &AdminRequest::SetRole {
                principal_id: principal_id.clone(),
                role,
            },
        )
        .await
        .unwrap();
        assert!(matches!(result, AdminResponse::Ok { .. }));
        assert_eq!(first.current_role().await, Ok(role));
        assert_eq!(second.current_role().await, Ok(role));
        assert_eq!(owner.current_role().await, Ok(WorkspaceRole::Owner));
    }
    let first_credential = first.stored_credential().unwrap();
    let second_credential = second.stored_credential().unwrap();
    let mut wrong_device = first.binding().clone();
    wrong_device.device_id = second.binding().device_id;
    let mut wrong_stream = first.binding().clone();
    wrong_stream.device_stream_id = second.binding().device_stream_id;
    let device = Session::from_stored_credential(
        &wrong_device,
        &credential_with_binding(&first_credential, &wrong_device),
    )
    .unwrap();
    let stream = Session::from_stored_credential(
        &wrong_stream,
        &credential_with_binding(&first_credential, &wrong_stream),
    )
    .unwrap();
    let mixed = Session::from_stored_credential(
        first.binding(),
        &credential_with_binding(&second_credential, first.binding()),
    )
    .unwrap();
    for result in [
        device.current_role().await,
        stream.current_role().await,
        mixed.current_role().await,
    ] {
        assert_eq!(result, Err(Error::Binding));
    }
    let result = admin::request(
        &config.server.admin_socket,
        &AdminRequest::RevokeDevice {
            device_id: first.binding().device_id.to_string(),
        },
    )
    .await
    .unwrap();
    assert!(matches!(result, AdminResponse::Ok { .. }));
    assert_eq!(first.current_role().await, Err(Error::Unauthorized));
    assert_eq!(second.current_role().await, Ok(WorkspaceRole::Editor));
    server.shutdown().await.unwrap();
}

async fn mock_role(case: &str) -> Result<WorkspaceRole, Error> {
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
        "device_stream_id": binding.device_stream_id.to_string(), "role": "editor"
    });
    let (status, media) = match case {
        "unsupported" => ("404 Not Found", "application/json"),
        "busy" => ("429 Too Many Requests", "application/json"),
        "unavailable" => ("503 Service Unavailable", "application/json"),
        "gateway_html" => ("404 Not Found", "text/html"),
        "gateway_forbidden" => ("403 Forbidden", "text/html"),
        "redirect" => ("302 Found", "text/html"),
        _ => ("200 OK", "application/json"),
    };
    match case {
        "reader" => body["role"] = "reader".into(),
        "owner" => body["role"] = "owner".into(),
        "protocol" => body["protocol"] = "mdw-sync/other".into(),
        "workspace" => body["workspace_id"] = Uuid::new_v4().to_string().into(),
        "epoch" => body["workspace_epoch"] = Uuid::new_v4().to_string().into(),
        "device" => body["device_id"] = Uuid::new_v4().to_string().into(),
        "stream" => body["device_stream_id"] = Uuid::new_v4().to_string().into(),
        "device_invalid" => body["device_id"] = "invalid".into(),
        "stream_invalid" => body["device_stream_id"] = "invalid".into(),
        "role_invalid" => body["role"] = "admin".into(),
        "missing" => {
            body.as_object_mut().unwrap().remove("role");
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
        assert!(request[..n].starts_with(b"GET /v1/devices/self/role "));
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
    let result = session.current_role().await;
    server.await.unwrap();
    result
}

#[tokio::test]
async fn current_role_rejects_malformed_and_foreign_responses() {
    assert_eq!(mock_role("ok").await, Ok(WorkspaceRole::Editor));
    assert_eq!(mock_role("reader").await, Ok(WorkspaceRole::Reader));
    assert_eq!(mock_role("owner").await, Ok(WorkspaceRole::Owner));
    for case in [
        "protocol",
        "device_invalid",
        "stream_invalid",
        "role_invalid",
        "missing",
        "extra",
    ] {
        assert_eq!(mock_role(case).await, Err(Error::Contract), "case={case}");
    }
    for case in ["workspace", "epoch", "device", "stream"] {
        assert_eq!(mock_role(case).await, Err(Error::Binding), "case={case}");
    }
    assert_eq!(mock_role("oversized").await, Err(Error::TooLarge));
}

#[tokio::test]
async fn current_role_distinguishes_unsupported_unavailable_and_gateway() {
    assert_eq!(mock_role("unsupported").await, Err(Error::RoleUnsupported));
    for case in ["busy", "unavailable"] {
        assert_eq!(
            mock_role(case).await,
            Err(Error::Unavailable),
            "case={case}"
        );
    }
    for case in ["gateway_html", "gateway_forbidden", "redirect"] {
        assert_eq!(
            mock_role(case).await,
            Err(Error::GatewayAuthRequired),
            "case={case}"
        );
    }
}
