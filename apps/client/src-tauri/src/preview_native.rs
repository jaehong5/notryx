use crate::{
    ProductSessionKind, Runtime, RuntimeState, check_caller, gate::RequestSnapshot,
    registration_binding,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use notryx_client_runtime::{Session, validate_preview_image};
use notryx_protocol::{WorkspacePreviewImage, WorkspacePreviewImageRequest};
use std::sync::Arc;
use tauri::Webview;
use tokio::sync::Mutex;

fn validate_editor(
    state: &RuntimeState,
    session: &Arc<Session>,
    snapshot: &RequestSnapshot,
    request: &WorkspacePreviewImageRequest,
) -> Result<(), String> {
    state.gate.validate(snapshot).map_err(str::to_owned)?;
    if !state
        .session
        .as_ref()
        .is_some_and(|current| Arc::ptr_eq(current, session))
    {
        return Err("stale_registration".into());
    }
    let edit = state
        .product_sessions
        .get(&request.edit_session_id)
        .ok_or("stale_session")?;
    if edit.document_id != request.document_id
        || edit.binding != registration_binding(session.binding())
        || !matches!(
            edit.kind,
            ProductSessionKind::Active | ProductSessionKind::Readonly
        )
    {
        return Err("stale_session".into());
    }
    Ok(())
}

pub async fn load_preview(
    state: Arc<Mutex<RuntimeState>>,
    request: &WorkspacePreviewImageRequest,
) -> Result<WorkspacePreviewImage, String> {
    let (session, snapshot, limit, slot) = {
        let state = state.lock().await;
        let snapshot = state
            .gate
            .snapshot(&request.renderer_session_id)
            .map_err(str::to_owned)?;
        let session = state.session.clone().ok_or("not_connected")?;
        validate_editor(&state, &session, &snapshot, request)?;
        let slot = state
            .preview_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| "preview_busy")?;
        (session, snapshot, state.preview_limit.clone(), slot)
    };
    let permit = limit.acquire_owned().await.map_err(|_| "preview_failed")?;
    {
        let state = state.lock().await;
        validate_editor(&state, &session, &snapshot, request)?;
    }
    let bytes = session
        .content(&request.node)
        .await
        .map_err(|error| format!("{error:?}"))?;
    {
        let state = state.lock().await;
        validate_editor(&state, &session, &snapshot, request)?;
    }
    let image = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let _slot = slot;
        validate_preview_image(&bytes)
    })
    .await
    .map_err(|_| "preview_failed")?
    .map_err(|_| "invalid_preview_image")?;
    let state = state.lock().await;
    validate_editor(&state, &session, &snapshot, request)?;
    Ok(WorkspacePreviewImage {
        png_base64: STANDARD.encode(image.png_bytes),
        width: image.width,
        height: image.height,
    })
}

#[tauri::command]
pub async fn workspace_preview_image(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    request: WorkspacePreviewImageRequest,
) -> Result<WorkspacePreviewImage, String> {
    check_caller(&webview)?;
    let image = load_preview(runtime.0.clone(), &request).await?;
    check_caller(&webview)?;
    Ok(image)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ProductSession, ProductSessionKind, RuntimeState, registration_binding};
    use base64::{Engine, engine::general_purpose::STANDARD};
    use notryx_client_runtime::Session;
    use notryx_client_store::ClientStore;
    use notryx_protocol::{AdminRequest, AdminResponse, AdminResult, WorkspacePreviewImageRequest};
    use notryx_server::{
        Workspace, admin,
        config::{Config, ServerConfig, WorkspaceConfig},
        http::RunningServer,
    };
    use std::{os::unix::fs::PermissionsExt, sync::Arc};
    use tokio::sync::Mutex;

    #[tokio::test]
    async fn only_current_editor_can_load_a_validated_server_image() {
        let directory = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let notes = directory.path().join("notes");
        let server_state = directory.path().join("server-state");
        std::fs::create_dir(&notes).unwrap();
        std::fs::create_dir(&server_state).unwrap();
        let mut workspace = Workspace::initialize(&notes, &server_state).await.unwrap();
        let png = STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==").unwrap();
        std::fs::write(notes.join("Document.md"), b"![image](Image.png)").unwrap();
        std::fs::write(notes.join("Image.png"), &png).unwrap();
        std::fs::write(notes.join("Bad.svg"), b"<svg onload='alert(1)'/>").unwrap();
        let document = workspace.adopt_root_document("Document.md").await.unwrap();
        let image = workspace.adopt_root_document("Image.png").await.unwrap();
        let bad = workspace.adopt_root_document("Bad.svg").await.unwrap();
        workspace.close().await.unwrap();
        let config = Config {
            server: ServerConfig {
                listen: "127.0.0.1:0".parse().unwrap(),
                admin_socket: directory.path().join("admin.sock"),
            },
            workspace: WorkspaceConfig {
                notes_root: notes,
                state_root: server_state,
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
            panic!("pairing")
        };
        let session = Arc::new(
            Session::pair(
                &format!("http://{}", server.local_addr()),
                workspace_id,
                &pairing_secret,
            )
            .await
            .unwrap(),
        );
        let nodes = session.inventory(None).await.unwrap().nodes;
        let node = nodes
            .iter()
            .find(|node| node.node_id == image.node_id)
            .unwrap()
            .clone();
        let bad_node = nodes
            .iter()
            .find(|node| node.node_id == bad.node_id)
            .unwrap()
            .clone();
        let document_node = nodes
            .iter()
            .find(|node| node.node_id == document.node_id)
            .unwrap();
        let binding = registration_binding(session.binding());
        let mut store = ClientStore::open(directory.path().join("client.sqlite3"))
            .await
            .unwrap();
        store.activate_registration(&binding).await.unwrap();
        let document_id = document.node_id.to_string();
        let edit = store
            .open_registered_document(
                &binding,
                &document_id,
                b"![image](Image.png)",
                document_node.content_rev.as_ref().unwrap().as_str(),
            )
            .await
            .unwrap();
        let mut state = RuntimeState::new(store, "owner".into());
        state.session = Some(session);
        state
            .gate
            .open_session(edit.edit_session_id.clone())
            .unwrap();
        state.product_sessions.insert(
            edit.edit_session_id.clone(),
            ProductSession {
                document_id: document_id.clone(),
                binding,
                kind: ProductSessionKind::Active,
            },
        );
        let state = Arc::new(Mutex::new(state));
        let request = WorkspacePreviewImageRequest {
            renderer_session_id: "owner".into(),
            document_id,
            edit_session_id: edit.edit_session_id,
            node,
        };
        let preview = load_preview(state.clone(), &request).await.unwrap();
        assert_eq!((preview.width, preview.height), (1, 1));
        assert!(
            STANDARD
                .decode(preview.png_base64)
                .unwrap()
                .starts_with(b"\x89PNG\r\n\x1a\n")
        );
        state
            .lock()
            .await
            .product_sessions
            .get_mut(&request.edit_session_id)
            .unwrap()
            .kind = ProductSessionKind::Readonly;
        assert!(load_preview(state.clone(), &request).await.is_ok());
        state
            .lock()
            .await
            .product_sessions
            .get_mut(&request.edit_session_id)
            .unwrap()
            .kind = ProductSessionKind::Recovery;
        assert_eq!(
            load_preview(state.clone(), &request).await,
            Err("stale_session".into())
        );
        state
            .lock()
            .await
            .product_sessions
            .get_mut(&request.edit_session_id)
            .unwrap()
            .kind = ProductSessionKind::Active;
        let mut invalid = request.clone();
        invalid.node = bad_node;
        assert_eq!(
            load_preview(state.clone(), &invalid).await,
            Err("invalid_preview_image".into())
        );
        let permit = state
            .lock()
            .await
            .preview_limit
            .clone()
            .acquire_owned()
            .await
            .unwrap();
        let pending = {
            let state = state.clone();
            let request = request.clone();
            tokio::spawn(async move { load_preview(state, &request).await })
        };
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(!pending.is_finished());
        drop(permit);
        assert!(pending.await.unwrap().is_ok());
        let slots = state
            .lock()
            .await
            .preview_slots
            .clone()
            .acquire_many_owned(8)
            .await
            .unwrap();
        assert_eq!(
            load_preview(state.clone(), &request).await,
            Err("preview_busy".into())
        );
        drop(slots);
        let permit = state
            .lock()
            .await
            .preview_limit
            .clone()
            .acquire_owned()
            .await
            .unwrap();
        let pending = {
            let state = state.clone();
            let request = request.clone();
            tokio::spawn(async move { load_preview(state, &request).await })
        };
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(!pending.is_finished());
        state.lock().await.gate.replace_owner("new".into());
        drop(permit);
        assert_eq!(pending.await.unwrap(), Err("stale_renderer_session".into()));
        assert_eq!(
            load_preview(state.clone(), &request).await,
            Err("stale_renderer_session".into())
        );
        server.shutdown().await.unwrap();
    }
}
