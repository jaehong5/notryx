use crate::{Runtime, RuntimeState, gate::RequestSnapshot};
use notryx_client_runtime::{Session, drawio_editor_xml, validate_preview_image};
use notryx_client_store::DiagramDraftCheckpoint;
use notryx_protocol::{
    DrawioCheckpoint, DrawioCheckpointRequest, DrawioEditorInput, DrawioExportRequest,
    WorkspacePreviewImageRequest,
};
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

#[derive(Clone)]
struct EditorSession {
    label: String,
    session: Arc<Session>,
    snapshot: RequestSnapshot,
    request: WorkspacePreviewImageRequest,
    input: Option<DrawioEditorInput>,
    base_xml: String,
    base_revision: String,
}

pub(crate) struct PreparedEditor {
    pub input: DrawioEditorInput,
    base_xml: String,
    base_revision: String,
}

#[derive(Default)]
pub(crate) struct EditorSessions(Mutex<Option<EditorSession>>);

impl EditorSessions {
    fn open(&self, state: &mut RuntimeState, editor: EditorSession) -> Result<(), String> {
        crate::preview_native::validate_editor(
            state,
            &editor.session,
            &editor.snapshot,
            &editor.request,
        )?;
        let mut current = self.0.lock().map_err(|_| "drawio_unavailable")?;
        if current.is_some() {
            return Err("drawio_busy".into());
        }
        // Reserve before creating the window so pairing cannot replace its registration.
        state
            .gate
            .open_session(editor.label.clone())
            .map_err(str::to_owned)?;
        *current = Some(editor);
        Ok(())
    }

    fn close(&self, gate: &mut crate::gate::RequestGate, label: &str) {
        if let Ok(mut current) = self.0.lock() {
            if current.as_ref().is_some_and(|editor| editor.label == label) {
                *current = None;
            }
            let _ = gate.close_session(label);
        }
    }
}

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
    let Some(runtime) = app.try_state::<Runtime>() else {
        return;
    };
    let state = runtime.0.clone();
    let app = app.clone();
    let label = label.to_owned();
    tauri::async_runtime::spawn(async move {
        let mut state = state.lock().await;
        if let Some(sessions) = app.try_state::<EditorSessions>() {
            sessions.close(&mut state.gate, &label);
        }
    });
}

pub(crate) fn cancel_all(app: &tauri::AppHandle) {
    if let Some(sessions) = app.try_state::<EditorSessions>()
        && let Ok(mut session) = sessions.0.lock()
        && let Some(editor) = session.take()
    {
        remove(app, &editor.label);
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
) -> Result<(Arc<Session>, RequestSnapshot, PreparedEditor), String> {
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
    let source_revision = request
        .node
        .content_rev
        .as_ref()
        .ok_or("drawio_invalid_input")?
        .as_str()
        .to_owned();
    let draft = {
        let mut state = state.lock().await;
        crate::preview_native::validate_editor(&state, &session, &snapshot, request)?;
        state
            .store
            .read_diagram_draft(
                &crate::registration_binding(session.binding()),
                &request.node.node_id.to_string(),
            )
            .await
            .map_err(|error| error.to_string())?
    };
    let (xml, base_xml, base_revision, generation, restored) = if let Some(draft) = draft {
        let xml =
            tokio::task::spawn_blocking(move || drawio_editor_xml(draft.xml.as_bytes(), false))
                .await
                .map_err(|_| "drawio_unavailable")?
                .map_err(|_| "drawio_invalid_input")?;
        (
            xml,
            draft.base_xml,
            draft.base_revision,
            draft.generation,
            true,
        )
    } else {
        (xml.clone(), xml, source_revision.clone(), 0, false)
    };
    let state = state.lock().await;
    crate::preview_native::validate_editor(&state, &session, &snapshot, request)?;
    Ok((
        session,
        snapshot,
        PreparedEditor {
            input: DrawioEditorInput {
                title: request.node.name.clone(),
                xml,
                generation: generation.to_string(),
                restored,
                source_changed: source_revision != base_revision,
            },
            base_xml,
            base_revision,
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
            let mut state = runtime.0.lock().await;
            let sessions = app.state::<EditorSessions>();
            sessions.open(
                &mut state,
                EditorSession {
                    label: label.clone(),
                    session,
                    snapshot,
                    request,
                    input: Some(input.input),
                    base_xml: input.base_xml,
                    base_revision: input.base_revision,
                },
            )?;
        }
        let result = WebviewWindowBuilder::new(&app, &label, WebviewUrl::App("drawio.html".into()))
            .title("Notryx 다이어그램 편집 (기기 초안)")
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
pub(crate) async fn drawio_checkpoint(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    sessions: tauri::State<'_, EditorSessions>,
    request: DrawioCheckpointRequest,
) -> Result<DrawioCheckpoint, String> {
    check_editor(&webview)?;
    checkpoint(runtime.0.clone(), &sessions, webview.label(), request).await
}

async fn checkpoint(
    state: Arc<tokio::sync::Mutex<RuntimeState>>,
    sessions: &EditorSessions,
    label: &str,
    request: DrawioCheckpointRequest,
) -> Result<DrawioCheckpoint, String> {
    let editor = sessions
        .0
        .lock()
        .map_err(|_| "drawio_unavailable")?
        .as_ref()
        .filter(|editor| editor.label == label && editor.input.is_none())
        .cloned()
        .ok_or("invalid_caller")?;
    let generation = request
        .generation
        .parse::<u64>()
        .map_err(|_| "invalid_generation")?;
    if generation.to_string() != request.generation {
        return Err("invalid_generation".into());
    }
    if request.xml.len() > 5 * 1024 * 1024 {
        return Err("drawio_invalid_input".into());
    }
    let xml = tokio::task::spawn_blocking(move || drawio_editor_xml(request.xml.as_bytes(), false))
        .await
        .map_err(|_| "drawio_unavailable")?
        .map_err(|_| "drawio_invalid_input")?;
    let mut state = state.lock().await;
    // A loaded diagram owns its draft even after its Markdown parent is closed.
    state
        .gate
        .validate(&editor.snapshot)
        .map_err(str::to_owned)?;
    if !state
        .session
        .as_ref()
        .is_some_and(|current| Arc::ptr_eq(current, &editor.session))
    {
        return Err("stale_registration".into());
    }
    if !sessions
        .0
        .lock()
        .map_err(|_| "drawio_unavailable")?
        .as_ref()
        .is_some_and(|current| current.label == label && current.input.is_none())
    {
        return Err("invalid_caller".into());
    }
    let generation = state
        .store
        .save_diagram_draft(
            &crate::registration_binding(editor.session.binding()),
            &editor.request.node.node_id.to_string(),
            &DiagramDraftCheckpoint {
                expected_generation: generation,
                xml,
                base_xml: editor.base_xml,
                base_revision: editor.base_revision,
            },
        )
        .await
        .map_err(|error| error.to_string())?;
    Ok(DrawioCheckpoint {
        generation: generation.to_string(),
    })
}

#[tauri::command]
pub(crate) fn drawio_close(webview: Webview, app: tauri::AppHandle) -> Result<(), String> {
    check_editor(&webview)?;
    #[cfg(not(target_os = "android"))]
    {
        if let Some(window) = app.get_webview_window(webview.label()) {
            window.destroy().map_err(|_| "drawio_unavailable")?;
        }
        remove(&app, webview.label());
    }
    Ok(())
}

#[cfg(any(target_os = "linux", test))]
fn validate_export_png(encoded: &str, xml: &str) -> Result<Vec<u8>, String> {
    use base64::{Engine, engine::general_purpose::STANDARD};
    const MAX_PNG_BYTES: usize = 25 * 1024 * 1024;
    if encoded.len() > MAX_PNG_BYTES.div_ceil(3) * 4 {
        return Err("drawio_export_invalid".into());
    }
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|_| "drawio_export_invalid")?;
    if bytes.len() > MAX_PNG_BYTES
        || drawio_editor_xml(&bytes, true).map_err(|_| "drawio_export_invalid")? != xml
    {
        return Err("drawio_export_invalid".into());
    }
    validate_preview_image(&bytes).map_err(|_| "drawio_export_invalid")?;
    // The preview is re-encoded without XML; export the validated original bytes.
    Ok(bytes)
}

#[cfg(any(target_os = "linux", test))]
async fn export_xml(
    state: &mut RuntimeState,
    sessions: &EditorSessions,
    label: &str,
    generation: u64,
) -> Result<String, String> {
    let editor = sessions
        .0
        .lock()
        .map_err(|_| "drawio_unavailable")?
        .as_ref()
        .filter(|editor| editor.label == label && editor.input.is_none())
        .cloned()
        .ok_or("invalid_caller")?;
    state
        .gate
        .validate(&editor.snapshot)
        .map_err(str::to_owned)?;
    if !state
        .session
        .as_ref()
        .is_some_and(|current| Arc::ptr_eq(current, &editor.session))
    {
        return Err("stale_registration".into());
    }
    let draft = state
        .store
        .read_diagram_draft(
            &crate::registration_binding(editor.session.binding()),
            &editor.request.node.node_id.to_string(),
        )
        .await
        .map_err(|error| error.to_string())?
        .ok_or("drawio_draft_missing")?;
    if draft.generation != generation {
        return Err("drawio_stale_export".into());
    }
    Ok(draft.xml)
}

#[tauri::command]
pub(crate) async fn drawio_export_png(
    webview: Webview,
    app: tauri::AppHandle,
    runtime: tauri::State<'_, Runtime>,
    request: DrawioExportRequest,
) -> Result<bool, String> {
    check_editor(&webview)?;
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (app, runtime, request);
        Err("drawio_export_unsupported".into())
    }
    #[cfg(target_os = "linux")]
    {
        let generation = request
            .generation
            .parse::<u64>()
            .map_err(|_| "invalid_generation")?;
        if generation == 0 || generation.to_string() != request.generation {
            return Err("invalid_generation".into());
        }
        let label = webview.label().to_owned();
        let sessions = app.state::<EditorSessions>();
        let (xml, permit) = {
            let mut state = runtime.0.lock().await;
            let permit = state
                .preview_limit
                .clone()
                .try_acquire_owned()
                .map_err(|_| "preview_busy")?;
            (
                export_xml(&mut state, &sessions, &label, generation).await?,
                permit,
            )
        };
        let bytes =
            tokio::task::spawn_blocking(move || validate_export_png(&request.png_base64, &xml))
                .await
                .map_err(|_| "drawio_export_failed")??;
        let Some(path) =
            crate::recovery_native::choose_export_path(&webview, "notryx-diagram.drawio.png")
                .await?
        else {
            return Ok(false);
        };
        let mut state = runtime.0.clone().lock_owned().await;
        export_xml(&mut state, &sessions, &label, generation).await?;
        tokio::task::spawn_blocking(move || {
            let _state = state;
            let _permit = permit;
            let sessions = app.state::<EditorSessions>();
            let editor = sessions.0.lock().map_err(|_| "drawio_unavailable")?;
            if !editor
                .as_ref()
                .is_some_and(|current| current.label == label && current.input.is_none())
            {
                return Err("invalid_caller".into());
            }
            crate::recovery_native::write_export(&path, &bytes).map_err(|error| {
                if error.kind() == std::io::ErrorKind::AlreadyExists {
                    "drawio_export_exists".to_owned()
                } else {
                    "drawio_export_failed".to_owned()
                }
            })?;
            Ok(true)
        })
        .await
        .map_err(|_| "drawio_export_failed")?
    }
}

#[cfg(test)]
pub(crate) mod tests {
    #[test]
    fn exported_png_must_preserve_the_checkpoint_and_have_valid_pixels() {
        use base64::{Engine, engine::general_purpose::STANDARD};
        let png =
            include_bytes!("../../../../crates/client-runtime/tests/fixtures/drawio-valid.png");
        let xml = notryx_client_runtime::drawio_editor_xml(png, true).unwrap();
        assert_eq!(
            super::validate_export_png(&STANDARD.encode(png), &xml).unwrap(),
            png
        );
        assert!(super::validate_export_png(&STANDARD.encode(png), "<mxfile/>").is_err());
        let mut broken = png.to_vec();
        broken[16..20].fill(0);
        assert!(notryx_client_runtime::drawio_editor_xml(&broken, true).is_ok());
        assert!(super::validate_export_png(&STANDARD.encode(broken), &xml).is_err());
        assert!(super::validate_export_png("not base64", &xml).is_err());
    }

    pub(crate) async fn checkpoint_roundtrip(
        state: std::sync::Arc<tokio::sync::Mutex<crate::RuntimeState>>,
        request: &notryx_protocol::WorkspacePreviewImageRequest,
    ) {
        use super::*;
        let (session, snapshot, prepared) = editor_input(state.clone(), request).await.unwrap();
        assert!(!prepared.input.restored);
        let xml = prepared.input.xml.replace("한글", "초안 복구");
        let sessions = EditorSessions::default();
        sessions
            .open(
                &mut *state.lock().await,
                EditorSession {
                    label: "drawio-test".into(),
                    session,
                    snapshot,
                    request: request.clone(),
                    input: None,
                    base_xml: prepared.base_xml.clone(),
                    base_revision: prepared.base_revision.clone(),
                },
            )
            .unwrap();
        let save = DrawioCheckpointRequest {
            generation: "0".into(),
            xml: xml.clone(),
        };
        assert!(
            checkpoint(state.clone(), &sessions, "main", save.clone())
                .await
                .is_err()
        );
        assert!(
            checkpoint(
                state.clone(),
                &sessions,
                "drawio-test",
                DrawioCheckpointRequest {
                    generation: "0".into(),
                    xml: "<!DOCTYPE mxfile><mxfile/>".into(),
                }
            )
            .await
            .is_err()
        );
        assert_eq!(
            checkpoint(state.clone(), &sessions, "drawio-test", save.clone())
                .await
                .unwrap()
                .generation,
            "1"
        );
        assert!(
            checkpoint(state.clone(), &sessions, "drawio-test", save)
                .await
                .is_err()
        );
        let (_, _, restored) = editor_input(state.clone(), request).await.unwrap();
        assert!(restored.input.restored);
        assert!(!restored.input.source_changed);
        assert_eq!(restored.input.xml, xml);
        assert_eq!(restored.input.generation, "1");
        assert_eq!(restored.base_xml, prepared.base_xml);
        assert_eq!(restored.base_revision, prepared.base_revision);
        let parent = state
            .lock()
            .await
            .product_sessions
            .remove(&request.edit_session_id)
            .unwrap();
        {
            let mut state = state.lock().await;
            state.gate.close_session(&request.edit_session_id).unwrap();
            assert!(
                matches!(state.gate.begin_pair("owner"), Err("open_edit_sessions")),
                "the diagram must prevent re-pairing after its Markdown parent closes"
            );
        }
        assert_eq!(
            checkpoint(
                state.clone(),
                &sessions,
                "drawio-test",
                DrawioCheckpointRequest {
                    generation: "1".into(),
                    xml: xml.clone(),
                }
            )
            .await
            .expect("the bound diagram remains editable after closing its Markdown parent")
            .generation,
            "2"
        );
        {
            let mut state = state.lock().await;
            assert_eq!(
                export_xml(&mut state, &sessions, "drawio-test", 2)
                    .await
                    .unwrap(),
                xml
            );
            assert!(
                export_xml(&mut state, &sessions, "drawio-test", 1)
                    .await
                    .is_err()
            );
            assert!(export_xml(&mut state, &sessions, "main", 2).await.is_err());
            state
                .product_sessions
                .insert(request.edit_session_id.clone(), parent);
            state
                .gate
                .open_session(request.edit_session_id.clone())
                .unwrap();
        }
        let previous_session = state.lock().await.session.take();
        assert!(
            export_xml(&mut *state.lock().await, &sessions, "drawio-test", 2)
                .await
                .is_err()
        );
        assert!(
            checkpoint(
                state.clone(),
                &sessions,
                "drawio-test",
                DrawioCheckpointRequest {
                    generation: "2".into(),
                    xml,
                }
            )
            .await
            .is_err()
        );
        state.lock().await.session = previous_session;
        assert_eq!(
            editor_input(state.clone(), request)
                .await
                .unwrap()
                .2
                .input
                .generation,
            "2"
        );
        {
            let mut state = state.lock().await;
            state.gate.close_session(&request.edit_session_id).unwrap();
            sessions.close(&mut state.gate, "drawio-test");
            assert!(sessions.0.lock().unwrap().is_none());
            let pair = state
                .gate
                .begin_pair("owner")
                .expect("closing the diagram permits re-pairing");
            state.gate.fail_pair(&pair).unwrap();
            state
                .gate
                .open_session(request.edit_session_id.clone())
                .unwrap();
        }
    }

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
            serde_json::json!([
                "allow-drawio-load",
                "allow-drawio-checkpoint",
                "allow-drawio-export-png",
                "allow-drawio-close"
            ])
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
        assert!(!permissions.contains(&serde_json::json!("allow-drawio-checkpoint")));
        assert!(!permissions.contains(&serde_json::json!("allow-drawio-close")));
    }
}
