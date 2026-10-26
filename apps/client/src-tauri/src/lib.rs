use base64::{Engine, engine::general_purpose::STANDARD};
use notryx_client_store::{ClientStore, MAX_DOCUMENT_BYTES};
use notryx_protocol::{
    M0CloseRequest, M0OpenRequest, M0OpenedDocument, M0SaveRequest, M0SavedCheckpoint,
};
use tauri::{Emitter, Manager, Webview};
#[cfg(not(target_os = "android"))]
use tauri::{WebviewUrl, WebviewWindowBuilder};
use tokio::sync::Mutex;

struct Runtime(Mutex<ClientStore>);

fn check_caller(webview: &Webview) -> Result<(), String> {
    let url = webview.url().map_err(|_| "invalid_caller")?;
    let local = (url.scheme() == "tauri" && url.host_str() == Some("localhost"))
        || (url.scheme() == "http" && url.host_str() == Some("tauri.localhost"))
        || (cfg!(debug_assertions)
            && url.scheme() == "http"
            && url.host_str() == Some("127.0.0.1")
            && url.port() == Some(1420));
    if webview.label() != "main" || !local || !matches!(url.path(), "" | "/" | "/index.html") {
        return Err("invalid_caller".into());
    }
    Ok(())
}

fn fixture(id: &str) -> Result<Vec<u8>, String> {
    let text = match id {
        "welcome" => "# Notryx\n\n이 문서는 M0 시험용 초안입니다.\n\n여기서 편집한 내용은 이 기기에만 저장됩니다.\n".to_owned(),
        "unicode" => "\u{feff}---\r\ncustom: preserve\r\n---\r\n# 한글 😀 e\u{301}\r\n\r\n```unknown\r\n[[합성 문서]]\r\n```\r\n<!-- keep -->".to_owned(),
        "mixed" => "# 혼합 줄바꿈\r\n원본\n보존\r\n".to_owned(),
        "non-utf8" => return Ok(vec![0xff, 0xfe, 0x00, 0xd8]),
        "100k" | "100k-b" | "5m" | "5m-b" => {
            let size = if id.starts_with("100k") { 100 * 1024 } else { MAX_DOCUMENT_BYTES };
            let mut text = "# 저장 성능 시험\n한글 😀 e\u{301}\n".to_owned();
            while text.len() + 65 <= size {
                text.push_str("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n");
            }
            text.push_str(&"x".repeat(size - text.len()));
            text
        }
        _ => return Err("unknown_fixture".into()),
    };
    Ok(text.into_bytes())
}

#[tauri::command]
async fn m0_open(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    request: M0OpenRequest,
) -> Result<M0OpenedDocument, String> {
    check_caller(&webview)?;
    let base = fixture(&request.document_id)?;
    let opened = runtime
        .0
        .lock()
        .await
        .open_document(&request.document_id, &base, "m0-fixture-v1")
        .await
        .map_err(|e| e.to_string())?;
    Ok(M0OpenedDocument {
        document_id: opened.document_id,
        edit_session_id: opened.edit_session_id,
        generation: opened.generation.to_string(),
        bytes_base64: STANDARD.encode(opened.bytes),
    })
}

#[tauri::command]
async fn m0_save(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    request: M0SaveRequest,
) -> Result<M0SavedCheckpoint, String> {
    check_caller(&webview)?;
    if matches!(request.document_id.as_str(), "mixed" | "non-utf8") {
        return Err("read_only_source".into());
    }
    if request.bytes_base64.len() > MAX_DOCUMENT_BYTES.div_ceil(3) * 4 {
        return Err("document_too_large".into());
    }
    let bytes = STANDARD
        .decode(&request.bytes_base64)
        .map_err(|_| "invalid_bytes")?;
    let generation = request
        .generation
        .parse::<u64>()
        .map_err(|_| "invalid_generation")?;
    let saved = runtime
        .0
        .lock()
        .await
        .save_checkpoint(
            &request.document_id,
            &request.edit_session_id,
            generation,
            &bytes,
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok(M0SavedCheckpoint {
        edit_session_id: saved.edit_session_id,
        generation: saved.generation.to_string(),
    })
}

#[tauri::command]
async fn m0_close(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    request: M0CloseRequest,
) -> Result<(), String> {
    check_caller(&webview)?;
    runtime
        .0
        .lock()
        .await
        .close_document(&request.document_id, &request.edit_session_id)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn m0_exit(webview: Webview, app: tauri::AppHandle) -> Result<(), String> {
    check_caller(&webview)?;
    app.exit(0);
    Ok(())
}

#[tauri::command]
fn m0_open_isolation(webview: Webview, app: tauri::AppHandle) -> Result<(), String> {
    check_caller(&webview)?;
    #[cfg(not(target_os = "android"))]
    if app.get_webview_window("renderer-probe").is_none() {
        WebviewWindowBuilder::new(
            &app,
            "renderer-probe",
            WebviewUrl::App("isolation.html".into()),
        )
        .title("Notryx renderer probe")
        .on_navigation(|url| {
            url.path() == "/isolation.html"
                && ((url.scheme() == "tauri" && url.host_str() == Some("localhost"))
                    || (cfg!(debug_assertions)
                        && url.scheme() == "http"
                        && url.host_str() == Some("127.0.0.1")
                        && url.port() == Some(1420)))
        })
        .build()
        .map_err(|_| "isolation_window_failed")?;
    }
    #[cfg(target_os = "android")]
    let _ = app;
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .setup(|app| {
            let directory = std::env::var_os("NOTRYX_M0_DATA_DIR")
                .map(std::path::PathBuf::from)
                .unwrap_or(app.path().app_data_dir()?);
            std::fs::create_dir_all(&directory)?;
            let store = tauri::async_runtime::block_on(ClientStore::open(
                directory.join("drafts.sqlite3"),
            ))?;
            app.manage(Runtime(Mutex::new(store)));
            Ok(())
        })
        .on_window_event(|window, event| {
            if window.label() == "main"
                && let tauri::WindowEvent::CloseRequested { api, .. } = event
            {
                api.prevent_close();
                let _ = window.emit("close-requested", ());
            }
        })
        .invoke_handler(tauri::generate_handler![
            m0_open,
            m0_save,
            m0_close,
            m0_exit,
            m0_open_isolation
        ])
        .run(tauri::generate_context!())
        .expect("client runtime failed");
}
