use crate::{ProductSession, Runtime, RuntimeState, check_caller};
use base64::{Engine, engine::general_purpose::STANDARD};
use notryx_protocol::WorkspaceRecoveryRequest;
use tauri::Webview;

const MAX_RECOVERY_BYTES: usize = 64 * 1024 * 1024;

fn validated_snapshot(
    state: &RuntimeState,
    request: &WorkspaceRecoveryRequest,
) -> Result<(ProductSession, Vec<u8>), String> {
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
    if request.bytes_base64.len() > MAX_RECOVERY_BYTES.div_ceil(3) * 4 {
        return Err("recovery_too_large".into());
    }
    let bytes = STANDARD
        .decode(&request.bytes_base64)
        .map_err(|_| "invalid_recovery_bytes")?;
    if bytes.len() > MAX_RECOVERY_BYTES {
        return Err("recovery_too_large".into());
    }
    Ok((session.clone(), bytes))
}

fn still_current(
    state: &RuntimeState,
    request: &WorkspaceRecoveryRequest,
    session: &ProductSession,
) -> Result<(), String> {
    state
        .gate
        .snapshot(&request.renderer_session_id)
        .map_err(str::to_owned)?;
    let current = state
        .product_sessions
        .get(&request.edit_session_id)
        .ok_or("stale_session")?;
    if current.document_id != session.document_id
        || current.binding != session.binding
        || current.kind != session.kind
    {
        return Err("stale_session".into());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub(crate) fn write_export(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let directory = path
        .parent()
        .ok_or_else(|| std::io::Error::other("missing parent"))?;
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    temporary
        .persist_noclobber(path)
        .map_err(|error| error.error)?;
    std::fs::File::open(directory)?.sync_all()
}

#[cfg(target_os = "linux")]
pub(crate) async fn choose_export_path(
    webview: &Webview,
    filename: &'static str,
) -> Result<Option<std::path::PathBuf>, String> {
    use gtk::prelude::*;
    let (sender, receiver) = tokio::sync::oneshot::channel();
    webview
        .run_on_main_thread(move || {
            let dialog = gtk::FileChooserNative::new(
                Some("원문 내보내기"),
                None::<&gtk::Window>,
                gtk::FileChooserAction::Save,
                Some("저장"),
                Some("취소"),
            );
            dialog.set_current_name(filename);
            dialog.set_do_overwrite_confirmation(false);
            let path = if dialog.run() == gtk::ResponseType::Accept {
                dialog.filename()
            } else {
                None
            };
            dialog.destroy();
            let _ = sender.send(path);
        })
        .map_err(|_| "recovery_dialog_failed")?;
    receiver.await.map_err(|_| "recovery_dialog_failed".into())
}

#[tauri::command]
pub async fn workspace_export_source(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    request: WorkspaceRecoveryRequest,
) -> Result<bool, String> {
    check_caller(&webview)?;
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (runtime, request);
        return Err("recovery_export_unsupported".into());
    }
    #[cfg(target_os = "linux")]
    {
        let (session, bytes) = {
            let state = runtime.0.lock().await;
            validated_snapshot(&state, &request)?
        };
        let Some(path) = choose_export_path(&webview, "notryx-recovery.md").await? else {
            return Ok(false);
        };
        let state = runtime.0.clone();
        tokio::task::spawn_blocking(move || {
            let state = state.blocking_lock();
            still_current(&state, &request, &session)?;
            write_export(&path, &bytes).map_err(|error| {
                if error.kind() == std::io::ErrorKind::AlreadyExists {
                    "recovery_export_exists".to_owned()
                } else {
                    "recovery_export_failed".to_owned()
                }
            })?;
            Ok(true)
        })
        .await
        .map_err(|_| "recovery_export_failed".to_owned())?
    }
}

#[tauri::command]
pub async fn workspace_copy_source(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    request: WorkspaceRecoveryRequest,
) -> Result<(), String> {
    check_caller(&webview)?;
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (runtime, request);
        return Err("recovery_copy_unsupported".into());
    }
    #[cfg(target_os = "linux")]
    {
        let state = runtime.0.clone().lock_owned().await;
        let (_session, bytes) = validated_snapshot(&state, &request)?;
        let text = String::from_utf8(bytes).map_err(|_| "recovery_copy_not_utf8")?;
        if text.contains('\0') {
            return Err("recovery_copy_contains_nul".into());
        }
        let (sender, receiver) = tokio::sync::oneshot::channel();
        webview
            .run_on_main_thread(move || {
                let clipboard = gtk::Clipboard::get(&gtk::gdk::SELECTION_CLIPBOARD);
                clipboard.set_text(&text);
                clipboard.store();
                drop(state);
                let _ = sender.send(());
            })
            .map_err(|_| "recovery_copy_failed")?;
        receiver.await.map_err(|_| "recovery_copy_failed".into())
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::{ProductSessionKind, RuntimeState};
    use notryx_client_store::{ClientStore, RegistrationBinding};
    use uuid::Uuid;

    #[tokio::test]
    async fn only_current_editor_can_export_its_supplied_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let store = ClientStore::open(dir.path().join("client.sqlite3"))
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
        let mut request = WorkspaceRecoveryRequest {
            renderer_session_id: "owner".into(),
            document_id: "document".into(),
            edit_session_id: "edit".into(),
            bytes_base64: STANDARD.encode(b"unsaved\n"),
        };
        let (session, bytes) = validated_snapshot(&state, &request).unwrap();
        assert_eq!(bytes, b"unsaved\n");
        assert!(still_current(&state, &request, &session).is_ok());
        request.document_id = "another".into();
        assert_eq!(
            validated_snapshot(&state, &request).err().as_deref(),
            Some("stale_session")
        );
        request.document_id = "document".into();
        state.gate.replace_owner("new".into());
        assert_eq!(
            still_current(&state, &request, &session).err().as_deref(),
            Some("stale_renderer_session")
        );
    }

    #[test]
    fn export_never_replaces_existing_files_or_symlinks() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("original");
        let chosen = dir.path().join("chosen");
        std::fs::write(&original, b"original").unwrap();
        symlink(&original, &chosen).unwrap();
        assert_eq!(
            write_export(&chosen, b"\xef\xbb\xbfunsent\r\n")
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::AlreadyExists
        );
        assert_eq!(std::fs::read(&original).unwrap(), b"original");
        assert!(
            std::fs::symlink_metadata(&chosen)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            write_export(&original, b"new").unwrap_err().kind(),
            std::io::ErrorKind::AlreadyExists
        );
        assert_eq!(std::fs::read(&original).unwrap(), b"original");
        let new = dir.path().join("new");
        write_export(&new, b"\xef\xbb\xbfunsent\r\n").unwrap();
        assert_eq!(std::fs::read(&new).unwrap(), b"\xef\xbb\xbfunsent\r\n");
    }

    #[test]
    fn concurrent_exports_never_replace_the_winner() {
        let dir = tempfile::tempdir().unwrap();
        let chosen = dir.path().join("chosen");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let tasks: Vec<_> = [b"first".as_slice(), b"second".as_slice()]
            .into_iter()
            .map(|bytes| {
                let path = chosen.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    write_export(&path, bytes)
                })
            })
            .collect();
        barrier.wait();
        let results: Vec<_> = tasks.into_iter().map(|task| task.join().unwrap()).collect();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter_map(|result| result.as_ref().err())
                .next()
                .unwrap()
                .kind(),
            std::io::ErrorKind::AlreadyExists
        );
        let bytes = std::fs::read(&chosen).unwrap();
        assert!(bytes == b"first" || bytes == b"second");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
