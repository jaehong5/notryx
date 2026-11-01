use super::{EditorSessions, check_editor, save_editor};
use crate::{Runtime, registration_binding, sync_catchup};
use notryx_client_runtime::{drawio_editor_xml, validate_preview_image};
use notryx_protocol::{DrawioConflictChoice, DrawioConflictRequest, DrawioEditorInput, NodeState};
use tauri::Webview;

#[derive(Clone)]
pub(super) struct PreparedRemote {
    choice: DrawioConflictChoice,
    node: NodeState,
    xml: String,
    response_generation: u64,
}

fn generation(value: &str) -> Result<u64, String> {
    value
        .parse::<u64>()
        .ok()
        .filter(|n| *n > 0 && n.to_string() == value)
        .ok_or_else(|| "invalid_generation".into())
}

async fn load_remote(
    runtime: &Runtime,
    sessions: &EditorSessions,
    label: &str,
    generation: u64,
) -> Result<PreparedRemote, String> {
    let (editor, binding, response_generation, copy, limit) = {
        let mut state = runtime.0.lock().await;
        let editor = save_editor(&state, sessions, label)?;
        let binding = registration_binding(editor.session.binding());
        let response_generation = state.transmission_gate.response_generation(&binding);
        if !state
            .transmission_gate
            .response_trusted(&binding, response_generation)
        {
            return Err("stale_sync".into());
        }
        let draft = state
            .store
            .read_diagram_draft(&binding, &editor.request.node.node_id.to_string())
            .await
            .map_err(|error| error.to_string())?
            .ok_or("drawio_draft_missing")?;
        if draft.generation != generation {
            return Err("drawio_stale_export".into());
        }
        let copy = state
            .store
            .read_diagram_conflict_copy(&binding, &editor.request.node.node_id.to_string())
            .await
            .map_err(|error| error.to_string())?
            .ok_or("drawio_copy_required")?;
        (
            editor,
            binding,
            response_generation,
            copy,
            state.preview_limit.clone(),
        )
    };
    sync_catchup::catch_up_for_generation(
        runtime,
        Some(&editor.snapshot),
        &editor.session,
        Some(response_generation),
    )
    .await
    .map_err(sync_catchup::CatchUpError::message)?;
    let node = {
        let mut state = runtime.0.lock().await;
        save_editor(&state, sessions, label)?;
        if !state
            .transmission_gate
            .response_trusted(&binding, response_generation)
        {
            return Err("stale_sync".into());
        }
        state
            .store
            .current_remote_file(&binding, editor.request.node.node_id)
            .await
            .map_err(|error| error.to_string())?
            .ok_or("remote_not_available")?
    };
    let png = node.name.to_ascii_lowercase().ends_with(".png");
    if !png && !node.name.to_ascii_lowercase().ends_with(".drawio") {
        return Err("drawio_unsupported_format".into());
    }
    let permit = tokio::time::timeout(std::time::Duration::from_secs(10), limit.acquire_owned())
        .await
        .map_err(|_| "preview_busy")?
        .map_err(|_| "drawio_unavailable")?;
    let bytes = editor
        .session
        .content(&node)
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
    let revision = node
        .content_rev
        .as_ref()
        .ok_or("drawio_invalid_input")?
        .as_str()
        .to_owned();
    let mut state = runtime.0.lock().await;
    save_editor(&state, sessions, label)?;
    let current = state
        .store
        .read_diagram_draft(&binding, &node.node_id.to_string())
        .await
        .map_err(|error| error.to_string())?
        .ok_or("drawio_draft_missing")?;
    let saved_copy = state
        .store
        .read_diagram_conflict_copy(&binding, &node.node_id.to_string())
        .await
        .map_err(|error| error.to_string())?
        .ok_or("drawio_copy_required")?;
    if current.generation != generation
        || saved_copy.operation_id != copy.operation_id
        || !state
            .transmission_gate
            .response_trusted(&binding, response_generation)
        || !state
            .store
            .remote_content_matches(&binding, &node)
            .await
            .map_err(|error| error.to_string())?
    {
        return Err("stale_conflict".into());
    }
    Ok(PreparedRemote {
        choice: DrawioConflictChoice {
            generation: generation.to_string(),
            operation_id: copy.operation_id,
            remote_revision: revision,
        },
        node,
        xml,
        response_generation,
    })
}

#[tauri::command]
pub(crate) async fn drawio_prepare_conflict_remote(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    sessions: tauri::State<'_, EditorSessions>,
    request: DrawioConflictRequest,
) -> Result<DrawioConflictChoice, String> {
    check_editor(&webview)?;
    prepare_remote(&runtime, &sessions, webview.label(), &request).await
}

async fn prepare_remote(
    runtime: &Runtime,
    sessions: &EditorSessions,
    label: &str,
    request: &DrawioConflictRequest,
) -> Result<DrawioConflictChoice, String> {
    let prepared = load_remote(runtime, sessions, label, generation(&request.generation)?).await?;
    let state = runtime.0.lock().await;
    save_editor(&state, sessions, label)?;
    let choice = prepared.choice.clone();
    let mut editors = sessions.0.lock().map_err(|_| "drawio_unavailable")?;
    let editor = editors
        .as_mut()
        .filter(|editor| editor.label == label)
        .ok_or("invalid_caller")?;
    editor.remote_choice = Some(prepared);
    Ok(choice)
}

#[tauri::command]
pub(crate) async fn drawio_choose_conflict_remote(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    sessions: tauri::State<'_, EditorSessions>,
    request: DrawioConflictChoice,
) -> Result<DrawioEditorInput, String> {
    check_editor(&webview)?;
    choose_remote(&runtime, &sessions, webview.label(), &request).await
}

async fn choose_remote(
    runtime: &Runtime,
    sessions: &EditorSessions,
    label: &str,
    request: &DrawioConflictChoice,
) -> Result<DrawioEditorInput, String> {
    let generation = generation(&request.generation)?;
    let prepared = {
        let state = runtime.0.lock().await;
        save_editor(&state, sessions, label)?
            .remote_choice
            .ok_or("stale_conflict")?
    };
    if prepared.choice != *request {
        return Err("stale_conflict".into());
    }
    let latest = load_remote(runtime, sessions, label, generation).await?;
    if latest.choice != prepared.choice
        || latest.xml != prepared.xml
        || latest.response_generation != prepared.response_generation
    {
        return Err("stale_conflict".into());
    }
    let mut state = runtime.0.lock().await;
    let editor = save_editor(&state, sessions, label)?;
    let binding = registration_binding(editor.session.binding());
    if !state
        .transmission_gate
        .response_trusted(&binding, prepared.response_generation)
        || !state
            .store
            .remote_content_matches(&binding, &latest.node)
            .await
            .map_err(|error| error.to_string())?
    {
        return Err("stale_conflict".into());
    }
    let draft = state
        .store
        .complete_diagram_conflict_choose_remote(
            &binding,
            &editor.request.node.node_id.to_string(),
            generation,
            &request.operation_id,
            &latest.xml,
            &request.remote_revision,
        )
        .await
        .map_err(|error| error.to_string())?;
    let input = DrawioEditorInput {
        title: latest.node.name.clone(),
        xml: draft.xml.clone(),
        generation: draft.generation.to_string(),
        restored: true,
        source_changed: false,
        can_save: true,
    };
    let mut editors = sessions.0.lock().map_err(|_| "drawio_unavailable")?;
    let current = editors
        .as_mut()
        .filter(|editor| editor.label == label)
        .ok_or("invalid_caller")?;
    current.request.node = latest.node;
    current.base_xml = draft.base_xml;
    current.base_revision = draft.base_revision;
    current.remote_choice = None;
    Ok(input)
}
