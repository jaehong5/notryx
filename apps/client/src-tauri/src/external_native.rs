use crate::{Runtime, RuntimeState, check_caller};
use notryx_client_runtime::{ExternalUrl, validate_external_url};
#[cfg(not(target_os = "android"))]
use notryx_client_runtime::{ExternalUrlError, open_external_url};
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

#[cfg(not(target_os = "android"))]
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
    #[cfg(target_os = "android")]
    {
        let url = validate_request(&*runtime.0.lock().await, &request)?;
        open_android(&webview, url).await
    }
    #[cfg(not(target_os = "android"))]
    {
        let state = runtime.0.clone();
        tokio::task::spawn_blocking(move || open_checked(&state, &request, open_external_url))
            .await
            .map_err(|_| "external_open_failed".to_owned())?
    }
}

#[cfg(target_os = "android")]
async fn open_android(webview: &Webview, url: ExternalUrl) -> Result<(), String> {
    let (complete, completed) = tokio::sync::oneshot::channel();
    webview
        .with_webview(move |native| {
            native.jni_handle().exec(move |env, activity, _| {
                if complete.is_closed() {
                    return;
                }
                let result = env
                    .with_local_frame(16, |env| start_android_intent(env, activity, &url))
                    .map_err(|_| {
                        // No installed handler is a recoverable error, not a pending JNI exception.
                        let _ = env.exception_clear();
                        "external_open_failed"
                    });
                let _ = complete.send(result);
            });
        })
        .map_err(|_| "external_open_failed")?;
    tokio::time::timeout(std::time::Duration::from_secs(5), completed)
        .await
        .map_err(|_| "external_open_failed")?
        .map_err(|_| "external_open_failed")?
        .map_err(str::to_owned)
}

#[cfg(target_os = "android")]
fn start_android_intent(
    env: &mut jni::JNIEnv<'_>,
    activity: &jni::objects::JObject<'_>,
    url: &ExternalUrl,
) -> jni::errors::Result<()> {
    use jni::objects::JValue;
    let target = env.new_string(url.as_str())?;
    let uri = env
        .call_static_method(
            "android/net/Uri",
            "parse",
            "(Ljava/lang/String;)Landroid/net/Uri;",
            &[JValue::Object(&target)],
        )?
        .l()?;
    let action = env.new_string("android.intent.action.VIEW")?;
    let intent = env.new_object(
        "android/content/Intent",
        "(Ljava/lang/String;Landroid/net/Uri;)V",
        &[JValue::Object(&action), JValue::Object(&uri)],
    )?;
    let category = env.new_string("android.intent.category.BROWSABLE")?;
    env.call_method(
        &intent,
        "addCategory",
        "(Ljava/lang/String;)Landroid/content/Intent;",
        &[JValue::Object(&category)],
    )?;
    env.call_method(
        activity,
        "startActivity",
        "(Landroid/content/Intent;)V",
        &[JValue::Object(&intent)],
    )?;
    Ok(())
}

#[cfg(all(test, not(target_os = "android")))]
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
            })?;
            assert_eq!(
                open_checked(&state, &request, |_| Err(ExternalUrlError::LaunchFailed)),
                Err("external_open_failed".into())
            );
            assert!(validate_request(&state.blocking_lock(), &request).is_ok());
            request.url = "intent://example.invalid/#Intent;scheme=https;end".into();
            assert_eq!(
                open_checked(&state, &request, |_| panic!("unsafe URL reached opener")),
                Err("invalid_external_url".into())
            );
            request.url = "https://example.invalid/".into();
            state.blocking_lock().product_sessions.clear();
            assert_eq!(
                open_checked(&state, &request, |_| panic!("closed editor reached opener")),
                Err("stale_session".into())
            );
            Ok::<(), String>(())
        })
        .await
        .unwrap()
        .unwrap();
    }
}
