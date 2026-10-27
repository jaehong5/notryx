use crate::{check_caller, gate::RequestGate, to_contract};
use base64::{Engine, engine::general_purpose::STANDARD};
use notryx_client_store::RecoveryStore;
use notryx_protocol::{
    ClientStartupMode, ClientStartupStatus, MigrationDraft, MigrationInventory,
    MigrationInventoryRequest, MigrationOperation, MigrationSource, MigrationSourceKind,
    MigrationSourceRequest,
};
use std::sync::Arc;
use tauri::Webview;
use tokio::sync::Mutex;

pub(crate) enum Startup {
    Ready,
    Recovery(Arc<Mutex<MigrationState>>),
    StoreLocked,
    Unavailable,
}

pub(crate) struct MigrationState {
    store: RecoveryStore,
    gate: RequestGate,
}

impl MigrationState {
    pub(crate) fn new(store: RecoveryStore, owner: String) -> Self {
        Self {
            store,
            gate: RequestGate::new(owner),
        }
    }

    pub(crate) fn replace_owner(&mut self, owner: String) {
        self.gate.replace_owner(owner);
    }
}

#[tauri::command]
pub async fn client_startup_status(
    webview: Webview,
    startup: tauri::State<'_, Startup>,
) -> Result<ClientStartupStatus, String> {
    check_caller(&webview)?;
    let (mode, renderer_session_id) = match startup.inner() {
        Startup::Ready => (ClientStartupMode::Ready, None),
        Startup::StoreLocked => (ClientStartupMode::StoreLocked, None),
        Startup::Unavailable => (ClientStartupMode::Unavailable, None),
        Startup::Recovery(state) => (
            ClientStartupMode::Recovery,
            Some(state.lock().await.gate.owner().to_owned()),
        ),
    };
    Ok(ClientStartupStatus {
        mode,
        renderer_session_id,
    })
}

fn recovery(startup: &Startup) -> Result<&Arc<Mutex<MigrationState>>, String> {
    match startup {
        Startup::Recovery(state) => Ok(state),
        _ => Err("recovery_not_available".into()),
    }
}

#[tauri::command]
pub async fn migration_inventory(
    webview: Webview,
    startup: tauri::State<'_, Startup>,
    request: MigrationInventoryRequest,
) -> Result<MigrationInventory, String> {
    check_caller(&webview)?;
    let mut state = recovery(startup.inner())?.lock().await;
    state
        .gate
        .snapshot(&request.renderer_session_id)
        .map_err(str::to_owned)?;
    recovery_inventory(&mut state.store, &request).await
}

async fn recovery_inventory(
    store: &mut RecoveryStore,
    request: &MigrationInventoryRequest,
) -> Result<MigrationInventory, String> {
    if (!request.include_drafts && request.draft_after_id.is_some())
        || (!request.include_operations && request.operation_after_id.is_some())
    {
        return Err("invalid_recovery_page".into());
    }
    let (draft_page, draft_error) = if request.include_drafts {
        match store.drafts(request.draft_after_id.as_deref()).await {
            Ok(page) => (Some(page), None),
            Err(error) => (None, Some(error.to_string())),
        }
    } else {
        (None, None)
    };
    let (operation_page, operation_error) = if request.include_operations {
        match store
            .operations(request.operation_after_id.as_deref())
            .await
        {
            Ok(page) => (Some(page), None),
            Err(error) => (None, Some(error.to_string())),
        }
    } else {
        (None, None)
    };
    let next_draft_after_id = draft_page
        .as_ref()
        .and_then(|page| page.next_after_id.clone());
    let next_operation_after_id = operation_page
        .as_ref()
        .and_then(|page| page.next_after_id.clone());
    let drafts = draft_page
        .into_iter()
        .flat_map(|page| page.items)
        .map(|draft| MigrationDraft {
            storage_id: draft.storage_id,
            document_id: draft.document_id,
            binding: draft.registration.map(to_contract),
            registration_status: draft.registration_status,
            generation: draft.generation.map(|value| value.to_string()),
            has_checkpoint: draft.has_checkpoint,
            has_base: draft.has_base,
            metadata_error: draft.metadata_error,
        })
        .collect();
    let operations = operation_page
        .into_iter()
        .flat_map(|page| page.items)
        .map(|operation| MigrationOperation {
            operation_id: operation.operation_id,
            document_id: operation.document_id,
            binding: operation.registration.map(to_contract),
            sequence: operation.sequence.map(|value| value.to_string()),
            generation: operation.generation.map(|value| value.to_string()),
            status: operation.status,
            terminal_kind: operation.terminal_kind,
            registration_status: operation.registration_status,
            metadata_error: operation.metadata_error,
        })
        .collect();
    Ok(MigrationInventory {
        drafts,
        operations,
        next_draft_after_id,
        next_operation_after_id,
        draft_error,
        operation_error,
    })
}

async fn source_bytes(
    store: &mut RecoveryStore,
    request: &MigrationSourceRequest,
) -> Result<Vec<u8>, String> {
    let id = request
        .id
        .as_deref()
        .filter(|id| !id.is_empty() && id.len() <= 128)
        .ok_or("invalid_recovery_source")?;
    match request.kind {
        MigrationSourceKind::Draft => store
            .checkpoint_bytes(id)
            .await
            .map_err(|error| error.to_string()),
        MigrationSourceKind::Base => store
            .base_bytes(id)
            .await
            .map_err(|error| error.to_string()),
        MigrationSourceKind::Outbox => store
            .operation_bytes(id)
            .await
            .map_err(|error| error.to_string()),
        MigrationSourceKind::Store => Err("invalid_recovery_source".into()),
    }
}

#[tauri::command]
pub async fn migration_source(
    webview: Webview,
    startup: tauri::State<'_, Startup>,
    request: MigrationSourceRequest,
) -> Result<MigrationSource, String> {
    check_caller(&webview)?;
    let mut state = recovery(startup.inner())?.lock().await;
    state
        .gate
        .snapshot(&request.renderer_session_id)
        .map_err(str::to_owned)?;
    let bytes = source_bytes(&mut state.store, &request).await?;
    Ok(MigrationSource {
        bytes_base64: STANDARD.encode(bytes),
    })
}

#[tauri::command]
pub async fn migration_export(
    webview: Webview,
    startup: tauri::State<'_, Startup>,
    request: MigrationSourceRequest,
) -> Result<bool, String> {
    check_caller(&webview)?;
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (startup, request);
        Err("recovery_export_unsupported".into())
    }
    #[cfg(target_os = "linux")]
    {
        let state = recovery(startup.inner())?.clone();
        {
            let state = state.lock().await;
            state
                .gate
                .snapshot(&request.renderer_session_id)
                .map_err(str::to_owned)?;
        }
        let filename = if request.kind == MigrationSourceKind::Store {
            "notryx-store-recovery.sqlite3"
        } else {
            "notryx-recovery.md"
        };
        let Some(path) = crate::recovery_native::choose_export_path(&webview, filename).await?
        else {
            return Ok(false);
        };
        let mut state = state.lock().await;
        state
            .gate
            .snapshot(&request.renderer_session_id)
            .map_err(str::to_owned)?;
        if request.kind == MigrationSourceKind::Store {
            if request.id.is_some() {
                return Err("invalid_recovery_source".into());
            }
            state.store.export_snapshot(path).await.map_err(|error| {
                if error == notryx_client_store::StoreError::ExportExists {
                    "recovery_export_exists".to_owned()
                } else {
                    error.to_string()
                }
            })?;
        } else {
            let bytes = source_bytes(&mut state.store, &request).await?;
            crate::recovery_native::write_export(&path, &bytes).map_err(|error| {
                if error.kind() == std::io::ErrorKind::AlreadyExists {
                    "recovery_export_exists"
                } else {
                    "recovery_export_failed"
                }
            })?;
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};

    async fn damaged_inventory(path: &std::path::Path, damage_drafts: bool) {
        let mut db = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(path)
                .create_if_missing(true),
        )
        .await
        .unwrap();
        sqlx::raw_sql("CREATE TABLE draft_checkpoints (document_id TEXT PRIMARY KEY, generation INTEGER, format_version INTEGER, bytes BLOB); CREATE TABLE draft_bases (document_id TEXT PRIMARY KEY, bytes BLOB, revision TEXT); CREATE TABLE registered_documents (registration_id INTEGER, document_id TEXT, storage_id TEXT); CREATE TABLE client_registrations (id INTEGER PRIMARY KEY, server_origin TEXT, workspace_id TEXT, workspace_epoch TEXT, device_id TEXT, device_stream_id TEXT, status TEXT); CREATE TABLE client_outbox (operation_id TEXT PRIMARY KEY, registration_id INTEGER, operation_seq INTEGER, document_id TEXT, generation INTEGER, bytes BLOB, status TEXT); INSERT INTO client_registrations VALUES (1, 'https://example.invalid', '11111111-1111-4111-8111-111111111111', '22222222-2222-4222-8222-222222222222', '33333333-3333-4333-8333-333333333333', '44444444-4444-4444-8444-444444444444', 'frozen'); INSERT INTO draft_checkpoints VALUES ('draft', 1, 1, X'61'); INSERT INTO draft_bases VALUES ('draft', X'62', 'rev'); INSERT INTO registered_documents VALUES (1, 'doc', 'draft'); INSERT INTO client_outbox VALUES ('op', 1, 1, 'doc', 1, X'62', 'sent'); PRAGMA application_id=1314148952; PRAGMA user_version=3;").execute(&mut db).await.unwrap();
        if damage_drafts {
            sqlx::query("DROP TABLE registered_documents")
                .execute(&mut db)
                .await
                .unwrap();
        } else {
            sqlx::query("DROP TABLE client_outbox")
                .execute(&mut db)
                .await
                .unwrap();
        }
        db.close().await.unwrap();
    }

    #[tokio::test]
    async fn corrupted_inventory_side_does_not_hide_the_other_side() {
        for damage_drafts in [true, false] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("drafts.db");
            damaged_inventory(&path, damage_drafts).await;
            let mut store = RecoveryStore::open(&path).await.unwrap();
            let result = recovery_inventory(
                &mut store,
                &MigrationInventoryRequest {
                    renderer_session_id: "owner".into(),
                    draft_after_id: None,
                    operation_after_id: None,
                    include_drafts: true,
                    include_operations: true,
                },
            )
            .await
            .unwrap();
            if damage_drafts {
                assert!(result.draft_error.is_some());
                assert_eq!(result.operations.len(), 1);
                assert!(result.operation_error.is_none());
            } else {
                assert!(result.operation_error.is_some());
                assert_eq!(result.drafts.len(), 1);
                assert!(result.draft_error.is_none());
            }
        }
    }
}
