use crate::{Runtime, RuntimeState, check_caller};
use notryx_client_runtime::{
    ExternalUrl, ExternalUrlError, open_external_url, validate_external_url,
};
use notryx_protocol::WorkspaceExternalLinkRequest;
use tauri::Webview;

fn validate_request(
    state: &RuntimeState,
    request: &WorkspaceExternalLinkRequest,
) -> Result<ExternalUrl, String> {
    state
        .gate
        .snapshot(&request.renderer_session_id)
        .map_err(str::to_owned)?;
    let session = state
        .product_sessions
        .get(&request.edit_session_id)
        .ok_or("stale_session")?;
    if session.document_id != request.document_id {
        return Err("stale_session".into());
    }
    validate_external_url(&request.url).map_err(|_| "invalid_external_url".into())
}

fn open_checked(
    state: &tokio::sync::Mutex<RuntimeState>,
    request: &WorkspaceExternalLinkRequest,
    opener: impl FnOnce(&ExternalUrl) -> Result<(), ExternalUrlError>,
) -> Result<(), String> {
    let url = {
        let state = state.blocking_lock();
        validate_request(&state, request)?
    };
    opener(&url).map_err(|_| "external_open_failed".into())
}

#[tauri::command]
pub async fn workspace_open_external(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    request: WorkspaceExternalLinkRequest,
) -> Result<(), String> {
    check_caller(&webview)?;
    let state = runtime.0.clone();
    tokio::task::spawn_blocking(move || open_checked(&state, &request, open_external_url))
        .await
        .map_err(|_| "external_open_failed".to_owned())?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ProductSession, ProductSessionKind, RuntimeState};
    use notryx_client_store::{ClientStore, RegistrationBinding};
    use notryx_protocol::WorkspaceExternalLinkRequest;
    use uuid::Uuid;

    #[tokio::test]
    async fn only_the_current_editor_can_request_a_safe_external_link() {
        let directory = tempfile::tempdir().unwrap();
        let store = ClientStore::open(directory.path().join("client.sqlite3"))
            .await
            .unwrap();
        let mut state = RuntimeState::new(store, "owner".into());
        let binding = RegistrationBinding {
            server_origin: "https://example.invalid".into(),
            workspace_id: Uuid::new_v4().to_string(),
            workspace_epoch: Uuid::new_v4().to_string(),
            device_id: Uuid::new_v4().to_string(),
            device_stream_id: Uuid::new_v4().to_string(),
        };
        state.product_sessions.insert(
            "edit".into(),
            ProductSession {
                document_id: "document".into(),
                binding,
                kind: ProductSessionKind::Recovery,
            },
        );
        let mut request = WorkspaceExternalLinkRequest {
            renderer_session_id: "owner".into(),
            document_id: "document".into(),
            edit_session_id: "edit".into(),
            url: "https://example.invalid/path".into(),
        };
        assert!(validate_request(&state, &request).is_ok());
        request.url = "https://user:secret@example.invalid/".into();
        assert_eq!(
            validate_request(&state, &request),
            Err("invalid_external_url".into())
        );
        request.url = "javascript:alert(1)".into();
        assert_eq!(
            validate_request(&state, &request),
            Err("invalid_external_url".into())
        );
        request.url = "https://example.invalid/".into();
        request.document_id = "another".into();
        assert_eq!(
            validate_request(&state, &request),
            Err("stale_session".into())
        );
        request.document_id = "document".into();
        state.gate.replace_owner("new".into());
        assert_eq!(
            validate_request(&state, &request),
            Err("stale_renderer_session".into())
        );
        request.renderer_session_id = "new".into();
        let state = std::sync::Arc::new(tokio::sync::Mutex::new(state));
        let check = state.clone();
        tokio::task::spawn_blocking(move || {
            open_checked(&state, &request, |_| {
                assert!(check.try_lock().is_ok());
                Ok(())
            })
        })
        .await
        .unwrap()
        .unwrap();
    }
}
