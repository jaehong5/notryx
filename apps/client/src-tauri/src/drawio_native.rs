use crate::{Runtime, RuntimeState, gate::RequestSnapshot};
use notryx_client_runtime::{Session, drawio_editor_xml, validate_preview_image};
use notryx_protocol::{DrawioEditorInput, WorkspacePreviewImageRequest};
use std::sync::{Arc, Mutex};
use tauri::{Manager, Webview};

const WRAPPER_CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; frame-src 'self'; connect-src ipc: http://ipc.localhost; img-src 'none'; object-src 'none'; base-uri 'none'; form-action 'none'";
fn editor_csp(origin: &str) -> String {
    format!(
        "default-src 'none'; script-src {origin} 'unsafe-eval'; style-src {origin} 'unsafe-inline'; img-src {origin} data: blob:; font-src {origin}; connect-src {origin}; frame-src 'none'; object-src 'none'; base-uri 'none'; form-action 'none'"
    )
}

// Wry registers the Linux asset scheme once per shared WebContext, on the first window.
pub(crate) fn asset_policy(
    request: tauri::http::Request<Vec<u8>>,
    response: &mut tauri::http::Response<std::borrow::Cow<'static, [u8]>>,
) {
    use tauri::http::HeaderValue;

    let path = request.uri().path();
    if path == "/drawio.html" {
        response.headers_mut().insert(
            "Content-Security-Policy",
            HeaderValue::from_static(WRAPPER_CSP),
        );
    } else if path.starts_with("/drawio/") {
        response.headers_mut().insert(
            "Content-Security-Policy",
            HeaderValue::from_str(&editor_csp(
                match (
                    request.uri().scheme_str(),
                    request
                        .uri()
                        .authority()
                        .map(|authority| authority.as_str()),
                ) {
                    (Some("http"), Some("tauri.localhost")) => "http://tauri.localhost",
                    (Some("http"), Some("127.0.0.1:1420")) if cfg!(debug_assertions) => {
                        "http://127.0.0.1:1420"
                    }
                    _ => "tauri://localhost",
                },
            ))
            .expect("static asset origin"),
        );
        response
            .headers_mut()
            .insert("Access-Control-Allow-Origin", HeaderValue::from_static("*"));
    }
}

struct EditorSession {
    label: String,
    session: Arc<Session>,
    snapshot: RequestSnapshot,
    request: WorkspacePreviewImageRequest,
    input: Option<DrawioEditorInput>,
}

#[derive(Default)]
pub(crate) struct EditorSessions(Mutex<Option<EditorSession>>);

fn editor_location(url: &tauri::Url) -> bool {
    let local = (url.scheme() == "tauri" && url.host_str() == Some("localhost"))
        || (url.scheme() == "http" && url.host_str() == Some("tauri.localhost"))
        || (cfg!(debug_assertions)
            && url.scheme() == "http"
            && url.host_str() == Some("127.0.0.1")
            && url.port() == Some(1420));
    local
        && url.path() == "/drawio.html"
        && url.query().is_none()
        && url.fragment().is_none()
        && url.username().is_empty()
        && url.password().is_none()
}

fn check_editor(webview: &Webview) -> Result<(), String> {
    if !webview.label().starts_with("drawio-")
        || !editor_location(&webview.url().map_err(|_| "invalid_caller")?)
    {
        return Err("invalid_caller".into());
    }
    Ok(())
}

fn editor_navigation(url: &tauri::Url) -> bool {
    if editor_location(url) {
        return true;
    }
    let mut origin = url.clone();
    origin.set_path("/drawio.html");
    origin.set_query(None);
    editor_location(&origin)
        && url.path() == "/drawio/index.html"
        && url.query()
            == Some(
                "embed=1&proto=json&offline=1&local=1&noSaveBtn=1&saveAndExit=0&noExitBtn=1&libraries=1&lang=ko&ui=kennedy&spin=1",
            )
}

pub(crate) fn remove(app: &tauri::AppHandle, label: &str) {
    if let Some(sessions) = app.try_state::<EditorSessions>()
        && let Ok(mut session) = sessions.0.lock()
        && session.as_ref().is_some_and(|s| s.label == label)
    {
        *session = None;
    }
}

pub(crate) fn cancel_all(app: &tauri::AppHandle) {
    if let Some(sessions) = app.try_state::<EditorSessions>()
        && let Ok(mut session) = sessions.0.lock()
    {
        *session = None;
    }
    #[cfg(not(target_os = "android"))]
    for (label, window) in app.webview_windows() {
        if label.starts_with("drawio-") {
            let _ = window.destroy();
        }
    }
}

pub(crate) async fn editor_input(
    state: Arc<tokio::sync::Mutex<RuntimeState>>,
    request: &WorkspacePreviewImageRequest,
) -> Result<(Arc<Session>, RequestSnapshot, DrawioEditorInput), String> {
    let (session, snapshot, limit) = {
        let state = state.lock().await;
        let snapshot = state
            .gate
            .snapshot(&request.renderer_session_id)
            .map_err(str::to_owned)?;
        let session = state.session.clone().ok_or("not_connected")?;
        crate::preview_native::validate_editor(&state, &session, &snapshot, request)?;
        (session, snapshot, state.preview_limit.clone())
    };
    let png = request.node.name.to_ascii_lowercase().ends_with(".png");
    if !png && !request.node.name.to_ascii_lowercase().ends_with(".drawio") {
        return Err("drawio_unsupported_format".into());
    }
    let permit = tokio::time::timeout(std::time::Duration::from_secs(10), limit.acquire_owned())
        .await
        .map_err(|_| "preview_busy")?
        .map_err(|_| "drawio_unavailable")?;
    {
        let state = state.lock().await;
        crate::preview_native::validate_editor(&state, &session, &snapshot, request)?;
    }
    let bytes = session
        .content(&request.node)
        .await
        .map_err(|error| format!("{error:?}"))?;
    let xml = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        if png {
            validate_preview_image(&bytes).map_err(|_| "invalid_preview_image")?;
        }
        drawio_editor_xml(&bytes, png).map_err(|_| "drawio_invalid_input")
    })
    .await
    .map_err(|_| "drawio_unavailable")??;
    let state = state.lock().await;
    crate::preview_native::validate_editor(&state, &session, &snapshot, request)?;
    Ok((
        session,
        snapshot,
        DrawioEditorInput {
            title: request.node.name.clone(),
            xml,
        },
    ))
}

#[tauri::command]
pub(crate) async fn drawio_open(
    webview: Webview,
    app: tauri::AppHandle,
    runtime: tauri::State<'_, Runtime>,
    request: WorkspacePreviewImageRequest,
) -> Result<(), String> {
    crate::check_caller(&webview)?;
    #[cfg(target_os = "android")]
    {
        let _ = (app, runtime, request);
        Err("drawio_unavailable".into())
    }
    #[cfg(not(target_os = "android"))]
    {
        use tauri::{WebviewUrl, WebviewWindowBuilder};
        let (session, snapshot, input) = editor_input(runtime.0.clone(), &request).await?;
        crate::check_caller(&webview)?;
        let label = format!("drawio-{}", uuid::Uuid::new_v4());
        {
            let state = runtime.0.lock().await;
            crate::preview_native::validate_editor(&state, &session, &snapshot, &request)?;
            let sessions = app.state::<EditorSessions>();
            let mut editor = sessions.0.lock().map_err(|_| "drawio_unavailable")?;
            if editor.is_some() {
                return Err("drawio_busy".into());
            }
            *editor = Some(EditorSession {
                label: label.clone(),
                session,
                snapshot,
                request,
                input: Some(input),
            });
        }
        let result = WebviewWindowBuilder::new(&app, &label, WebviewUrl::App("drawio.html".into()))
            .title("Notryx 다이어그램 시험 편집 (저장 안 됨)")
            .inner_size(1100.0, 800.0)
            .disable_drag_drop_handler()
            .on_navigation(editor_navigation)
            .on_new_window(|_, _| tauri::webview::NewWindowResponse::Deny)
            .on_web_resource_request(asset_policy)
            .build();
        if result.is_err() {
            remove(&app, &label);
            return Err("drawio_unavailable".into());
        }
        let valid = {
            let state = runtime.0.lock().await;
            let sessions = app.state::<EditorSessions>();
            let editor = sessions.0.lock().map_err(|_| "drawio_unavailable")?;
            editor.as_ref().is_some_and(|editor| {
                editor.label == label
                    && crate::preview_native::validate_editor(
                        &state,
                        &editor.session,
                        &editor.snapshot,
                        &editor.request,
                    )
                    .is_ok()
            })
        };
        if !valid {
            if let Some(window) = app.get_webview_window(&label) {
                let _ = window.destroy();
            }
            remove(&app, &label);
            return Err("stale_session".into());
        }
        Ok(())
    }
}

#[tauri::command]
pub(crate) async fn drawio_load(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    sessions: tauri::State<'_, EditorSessions>,
) -> Result<DrawioEditorInput, String> {
    check_editor(&webview)?;
    let state = runtime.0.lock().await;
    let mut editor = sessions.0.lock().map_err(|_| "drawio_unavailable")?;
    let editor = editor
        .as_mut()
        .filter(|s| s.label == webview.label())
        .ok_or("invalid_caller")?;
    crate::preview_native::validate_editor(
        &state,
        &editor.session,
        &editor.snapshot,
        &editor.request,
    )?;
    editor.input.take().ok_or("drawio_already_loaded".into())
}

#[tauri::command]
pub(crate) fn drawio_close(webview: Webview, app: tauri::AppHandle) -> Result<(), String> {
    check_editor(&webview)?;
    #[cfg(not(target_os = "android"))]
    {
        remove(&app, webview.label());
        if let Some(window) = app.get_webview_window(webview.label()) {
            window.destroy().map_err(|_| "drawio_unavailable")?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn only_the_bundled_wrapper_can_claim_a_session() {
        assert!(super::editor_location(
            &"tauri://localhost/drawio.html".parse().unwrap()
        ));
        for url in [
            "tauri://localhost/index.html",
            "tauri://localhost/drawio/index.html",
            "tauri://localhost/drawio.html?node=other",
            "tauri://localhost/drawio.html#other",
            "https://example.invalid/drawio.html",
            "file:///drawio.html",
        ] {
            assert!(!super::editor_location(&url.parse().unwrap()));
        }
        let cap: serde_json::Value =
            serde_json::from_str(include_str!("../capabilities/drawio.json")).unwrap();
        assert_eq!(cap["windows"], serde_json::json!(["drawio-*"]));
        assert_eq!(
            cap["permissions"],
            serde_json::json!(["allow-drawio-load", "allow-drawio-close"])
        );
        assert!(cap.get("remote").is_none());
    }

    #[test]
    fn iframe_navigation_does_not_grant_native_permissions_or_remote_access() {
        let url: tauri::Url = "tauri://localhost/drawio/index.html?embed=1&proto=json&offline=1&local=1&noSaveBtn=1&saveAndExit=0&noExitBtn=1&libraries=1&lang=ko&ui=kennedy&spin=1".parse().unwrap();
        assert!(super::editor_navigation(&url));
        assert!(!super::editor_location(&url));
        let mut bad = url.clone();
        bad.set_query(Some("embed=1&url=https://example.invalid/file"));
        assert!(!super::editor_navigation(&bad));
        bad = url;
        bad.set_fragment(Some("remote"));
        assert!(!super::editor_navigation(&bad));
        let csp = super::editor_csp("tauri://localhost");
        assert!(!csp.contains("127.0.0.1"));
        assert!(!csp.contains("ipc:"));
        assert!(csp.contains("frame-src 'none'"));
    }

    #[test]
    fn shared_asset_policy_only_changes_diagram_resources() {
        for (path, changed) in [
            ("index.html", false),
            ("mermaid.html", false),
            ("drawio/index.html", true),
        ] {
            let request = tauri::http::Request::builder()
                .uri(format!("tauri://localhost/{path}"))
                .body(Vec::new())
                .unwrap();
            let mut response = tauri::http::Response::builder()
                .header("Content-Security-Policy", "default-src 'none'")
                .body(std::borrow::Cow::Borrowed(&[][..]))
                .unwrap();
            super::asset_policy(request, &mut response);
            assert_eq!(
                response
                    .headers()
                    .contains_key("Access-Control-Allow-Origin"),
                changed
            );
            if !changed {
                assert_eq!(
                    response.headers()["Content-Security-Policy"],
                    "default-src 'none'"
                );
            }
        }
    }

    #[test]
    fn main_can_open_the_editor_but_cannot_claim_its_source() {
        let main: serde_json::Value =
            serde_json::from_str(include_str!("../capabilities/main.json")).unwrap();
        let permissions = main["permissions"].as_array().unwrap();
        assert!(permissions.contains(&serde_json::json!("allow-drawio-open")));
        assert!(!permissions.contains(&serde_json::json!("allow-drawio-load")));
        assert!(!permissions.contains(&serde_json::json!("allow-drawio-close")));
    }
}
